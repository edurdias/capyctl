//! ADR 0013 (unit I2): one deployment with two instances placed across two
//! enrolled hosts, driven through the real `mllm` binary: a server and two host
//! roles on this machine, each host running a fake vLLM executable behind its
//! own private ingress.
//!
//! Resolution against every allowed host and the label selector, spread
//! placement, per-instance status, `stop instance` / `start instance`, a host
//! drain that stops only the instance on that host, and a count decrease that
//! retires one instance with verified cleanup all go through the product's own
//! paths.
//!
//! CPU and fake-engine tests are not qualification: passing this never shows a
//! native engine recipe works on either Spark (SPEC §18). The live steps that
//! prove the same on real engines are in the I2 hand-off.

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
        answer = "fake-vllm-answer from " + host
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

/// The first of two consecutive loopback ports that are free now, below the
/// ephemeral range (`support::process::free_ports`).
fn free_port_pair() -> u16 {
    support::process::free_ports(2, true)[0]
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
        std::fs::write(&server_config, serde_json::to_vec(&server).unwrap()).unwrap();
        let models = path.join("models");
        std::fs::create_dir_all(models.join("toy")).unwrap();
        std::fs::set_permissions(&models, std::fs::Permissions::from_mode(0o700)).unwrap();
        let golden: Value = serde_json::from_str(include_str!(
            "../../mllm-config/tests/fixtures/effective-vllm-golden.json"
        ))
        .unwrap();
        let hosts = [("i2-spark-a", "a"), ("i2-spark-b", "b")]
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
                std::fs::write(
                    engine.join("vllm"),
                    format!("#!{}\n{FAKE_VLLM}", python3().display()),
                )
                .unwrap();
                std::fs::set_permissions(
                    engine.join("vllm"),
                    std::fs::Permissions::from_mode(0o755),
                )
                .unwrap();
                std::fs::write(engine.join("host"), name).unwrap();
                std::os::unix::fs::symlink(python3(), engine.join("python3")).unwrap();
                // SPEC §13.3: the runtime directory the host trusts is its own
                // and writable by it alone.
                let runtime = path.join(format!("{name}-runtime"));
                std::fs::create_dir_all(&runtime).unwrap();
                std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
                private_file(&runtime.join("mllm_vllm_guard.py"), "# test guard\n");
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
                // Two consecutive ports: a host with per-launch claims may run
                // two co-resident launches (SPEC §§3.1, 7.3).
                let port = free_port_pair();
                host["resource_policy"]["endpoint_port_range"] =
                    json!({"start": port, "end": port + 1});
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

    /// `PUT /management/v1/deployments/{id}`: a new revision of the deployment.
    async fn replace(&self, id: &str, revision: i64, config: &Value) -> (u16, Value) {
        let server: Value =
            serde_json::from_slice(&std::fs::read(&self.server_config).unwrap()).unwrap();
        let management = server["listeners"]["management"]["bind"].as_str().unwrap();
        let response = reqwest::Client::new()
            .put(format!(
                "http://{management}/management/v1/deployments/{id}"
            ))
            .bearer_auth(self.credential("admin_token"))
            .header("idempotency-key", format!("replace-{revision}"))
            .json(&json!({"config": config, "expected_revision": revision}))
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn chat(&self) -> (u16, String) {
        self.chat_model("toy").await
    }

    async fn chat_model(&self, model: &str) -> (u16, String) {
        let response = reqwest::Client::new()
            .post(format!("http://{}/v1/chat/completions", self.inference))
            .bearer_auth(self.api_key())
            .json(&json!({"model": model, "messages": [{"role": "user", "content": "hello"}]}))
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

    /// Wait for an accepted operation to succeed.
    fn succeeded(&self, operation: &str) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let snapshot = mllm_store::Store::open(&self.server_state.join("srv.sqlite3"))
                .ok()
                .and_then(|store| store.snapshot().ok());
            if snapshot.is_some_and(|snapshot| {
                snapshot
                    .operations
                    .iter()
                    .any(|op| op.id == operation && op.state == "succeeded")
            }) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "operation {operation} never succeeded"
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

impl Cluster {
    /// Enroll and start every host role; wait until all are eligible.
    fn join_hosts(&self) -> (Vec<Role>, Value) {
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
            roles.push(self.start("host", &host.state, &host.config));
        }
        let listed = self.eligible_hosts(self.hosts.len());
        (roles, listed)
    }

    /// A small restart-only deployment of the golden vLLM recipe, serving
    /// `route`, whose instances share the host's device.
    fn deployment(&self, route: &str) -> Value {
        let golden: Value = serde_json::from_str(include_str!(
            "../../mllm-config/tests/fixtures/effective-vllm-golden.json"
        ))
        .unwrap();
        let mut deployment = golden["input"]["deployment"].clone();
        deployment["name"] = json!(route);
        deployment["routes"] = json!([route]);
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
        deployment
    }

    /// `deploy model --activate --wait` for `deployment`; its id.
    fn deploy(&self, deployment: &Value, file: &str) -> String {
        let file = self.root.path().join(file);
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
    let mut command = support::mllm();
    command
        .env("MLLM_STATE_DIR", state)
        .env_remove("MLLM_VLLM_BIN")
        .env_remove("MLLM_SGLANG_BIN");
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

/// T05 T10 T15 T16 T27 T29 T33: two instances of one deployment placed across
/// two enrolled hosts by the scheduler; each instance stops, starts and drains
/// on its own, and a count decrease retires one with verified cleanup.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_instances_are_placed_across_two_hosts_and_managed_one_at_a_time() {
    let cluster = Cluster::new();
    let server = cluster.start("server", &cluster.server_state, &cluster.server_config);
    let deadline = Instant::now() + Duration::from_secs(30);
    while !cluster
        .manage(&["list", "hosts", "--format", "json"])
        .status
        .success()
    {
        assert!(Instant::now() < deadline, "the server never answered");
        std::thread::sleep(Duration::from_millis(100));
    }
    let mut roles = Vec::new();
    for host in &cluster.hosts {
        let invitation = cluster.root.path().join(format!("{}.join", host.name));
        let out = cluster.manage(&[
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
        roles.push(cluster.start("host", &host.state, &host.config));
    }
    let listed = cluster.eligible_hosts(2);
    let host_id = |name: &str| {
        listed["hosts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["host_name"] == name || h["name"] == name)
            .and_then(|h| h["host_id"].as_str())
            .unwrap_or_else(|| panic!("host {name} listed: {listed}"))
            .to_owned()
    };
    let (a, b) = (host_id("i2-spark-a"), host_id("i2-spark-b"));

    // ADR 0013 §2: two instances, one per host, on hosts whose labels match;
    // devices stated by sharing mode because a device id is host-local.
    let golden: Value = serde_json::from_str(include_str!(
        "../../mllm-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    let mut deployment = golden["input"]["deployment"].clone();
    deployment["instances"] = json!(2);
    deployment["placement"] = json!({
        "hosts": ["i2-spark-a", "i2-spark-b"],
        "selector": {"gpu": "gb10"},
        "strategy": "spread",
        "max_per_host": 1
    });
    deployment["runtime_profile_revision"] =
        golden["input"]["host"]["runtime_profiles"]["local"]["revision"].clone();
    deployment["model"] = json!({
        "source": {"type": "local", "path": cluster.root.path().join("models/toy")},
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
    let file = cluster.root.path().join("deployment.json");
    std::fs::write(&file, deployment.to_string()).unwrap();
    let deployed = cluster.manage_json(&[
        "deploy",
        "model",
        "--file",
        file.to_str().unwrap(),
        "--activate",
        "--wait",
    ]);
    let id = deployed["deployment"]["id"].as_str().unwrap().to_owned();

    // Spread: one instance on each host, each with its own engine.
    let status = cluster.ready_instances(&id, 2);
    let placed: Vec<&str> = status["instances"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["host_id"].as_str().unwrap())
        .collect();
    let mut expected = vec![a.as_str(), b.as_str()];
    let mut sorted = placed.clone();
    sorted.sort();
    expected.sort();
    assert_eq!(sorted, expected, "{status}");
    assert_eq!(status["observed_state"], "ready");
    assert!(
        status["conditions"].as_array().unwrap().is_empty(),
        "{status}"
    );
    assert_eq!(
        status["hosts"].as_array().unwrap().len(),
        2,
        "both allowed hosts resolved: {status}"
    );
    for host in &cluster.hosts {
        assert_eq!(host.launches().len(), 1, "{} ran one engine", host.name);
    }
    cluster.served(Duration::from_secs(30)).await;

    // Owner decision Q7: stop the instance on host b; host a keeps serving.
    let one = status["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["host_id"] == b.as_str())
        .unwrap()["index"]
        .as_u64()
        .unwrap();
    let engine_b = cluster.hosts[1].launches()[0];
    let stopped = cluster.manage_json(&[
        "stop",
        "instance",
        &format!("{id}/{one}"),
        "--format",
        "json",
    ]);
    cluster.succeeded(stopped["operation_id"].as_str().unwrap());
    assert!(!alive(engine_b), "the stopped instance's engine is gone");
    let status = cluster.ready_instances(&id, 1);
    assert_eq!(status["observed_state"], "ready");
    // The operator stopped it: nothing else is wanted, so not `degraded`.
    assert_eq!(status["conditions"], json!([]), "{status}");
    assert_eq!(status["instances"][one as usize]["operator_stopped"], true);
    assert!(cluster
        .served(Duration::from_secs(10))
        .await
        .contains("i2-spark-a"));

    // `start instance` lifts the stop; the instance returns to its host.
    let started = cluster.manage_json(&[
        "start",
        "instance",
        &format!("{id}/{one}"),
        "--format",
        "json",
    ]);
    cluster.succeeded(started["operation_id"].as_str().unwrap());
    let status = cluster.ready_instances(&id, 2);
    assert_eq!(cluster.hosts[1].launches().len(), 2);
    assert!(status["instances"]
        .as_array()
        .unwrap()
        .iter()
        .any(|i| i["index"] == json!(one) && i["host_id"] == b.as_str()));

    // SPEC §4.3: draining host a stops only the instance on host a.
    let engine_a = cluster.hosts[0].launches()[0];
    let drained = cluster.manage_json(&["drain", "host", "i2-spark-a"]);
    assert_eq!(drained["drained"], true, "{drained}");
    assert_eq!(
        drained["deployments"].as_array().unwrap().len(),
        1,
        "{drained}"
    );
    assert!(!alive(engine_a));
    let status = cluster.ready_instances(&id, 1);
    assert_eq!(status["observed_state"], "ready");
    assert!(cluster
        .served(Duration::from_secs(10))
        .await
        .contains("i2-spark-b"));

    // ADR 0013 §7: a count decrease is a revision; the stopped instance is the
    // one retired, and the running one keeps serving untouched.
    let engine_b = *cluster.hosts[1].launches().last().unwrap();
    let mut smaller = deployment.clone();
    smaller["instances"] = json!(1);
    let (code, replaced) = cluster.replace(&id, 1, &smaller).await;
    assert_eq!(code, 202, "{replaced}");
    assert_eq!(replaced["revision"], "2");
    let status = cluster.ready_instances(&id, 1);
    let instances = status["instances"].as_array().unwrap();
    assert_eq!(
        instances.len(),
        1,
        "the stopped instance was retired: {status}"
    );
    assert_eq!(instances[0]["host_id"], b.as_str());
    assert_eq!(
        instances[0]["revision"], "1",
        "the running instance kept its incarnation"
    );
    assert!(alive(engine_b), "the running instance was not disturbed");
    cluster.served(Duration::from_secs(10)).await;

    // An operator stop of the deployment stops what runs, with cleanup.
    let stopped = cluster.manage_json(&["stop", "deployment", &id, "--format", "json"]);
    cluster.succeeded(stopped["operation_id"].as_str().unwrap());
    assert!(!alive(engine_b));
    let status = cluster.ready_instances(&id, 0);
    assert_eq!(status["observed_state"], "stopped", "{status}");

    for role in roles {
        role.stop();
    }
    server.stop();
    let store = mllm_store::Store::open(&cluster.server_state.join("srv.sqlite3")).unwrap();
    assert!(store.pending_dispatches(&id).unwrap().is_empty());
    assert!(store.resource_snapshot().unwrap().owners.is_empty());
}

/// SPEC §§3.1, 7.3 (per-launch host claims), owner goal D10: two deployments
/// pinned to one enrolled host are Ready there at once through the product
/// (the host agent advertises per-launch claims, so the scheduler places the
/// second beside the first), each served by its own route, and each stops on
/// its own with verified cleanup while the other keeps serving.
// T24 T26 T27 T10 T33
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_deployments_co_reside_on_one_enrolled_host() {
    let cluster = Cluster::new();
    let server = cluster.start("server", &cluster.server_state, &cluster.server_config);
    let deadline = Instant::now() + Duration::from_secs(30);
    while !cluster
        .manage(&["list", "hosts", "--format", "json"])
        .status
        .success()
    {
        assert!(Instant::now() < deadline, "the server never answered");
        std::thread::sleep(Duration::from_millis(100));
    }
    let (roles, listed) = cluster.join_hosts();
    let a = listed["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["host_name"] == "i2-spark-a" || h["name"] == "i2-spark-a")
        .and_then(|h| h["host_id"].as_str())
        .unwrap()
        .to_owned();

    let mut first = cluster.deployment("toy");
    first["host"] = json!("i2-spark-a");
    let mut second = cluster.deployment("toy2");
    second["host"] = json!("i2-spark-a");
    let first = cluster.deploy(&first, "first.json");
    let second = cluster.deploy(&second, "second.json");
    for id in [&first, &second] {
        let status = cluster.ready_instances(id, 1);
        assert_eq!(status["instances"][0]["host_id"], a.as_str(), "{status}");
    }
    let host_a = &cluster.hosts[0];
    assert_eq!(host_a.launches().len(), 2, "two engines on one host");
    assert!(cluster.hosts[1].launches().is_empty());
    for model in ["toy", "toy2"] {
        let (status, body) = cluster.chat_model(model).await;
        assert_eq!(status, 200, "{model}: {body}");
        assert!(body.contains("fake-vllm-answer from i2-spark-a"), "{body}");
    }

    // Each stops on its own: only its engine is proved gone.
    let engines = host_a.launches();
    let stopped = cluster.manage_json(&["stop", "deployment", &first, "--format", "json"]);
    cluster.succeeded(stopped["operation_id"].as_str().unwrap());
    assert_eq!(engines.iter().filter(|pid| alive(**pid)).count(), 1);
    assert_eq!(
        cluster.ready_instances(&first, 0)["observed_state"],
        "stopped"
    );
    let (status, body) = cluster.chat_model("toy2").await;
    assert_eq!(status, 200, "{body}");
    let stopped = cluster.manage_json(&["stop", "deployment", &second, "--format", "json"]);
    cluster.succeeded(stopped["operation_id"].as_str().unwrap());
    assert!(engines.iter().all(|pid| !alive(*pid)));

    for role in roles {
        role.stop();
    }
    server.stop();
    let store = mllm_store::Store::open(&cluster.server_state.join("srv.sqlite3")).unwrap();
    assert!(store.resource_snapshot().unwrap().owners.is_empty());
}

/// ADR 0013 §4, §5 (owner decision P1; per-instance host fencing): two
/// instances of one deployment pinned to one enrolled host are Ready there at
/// once, because the host agent fences each instance by its own generation and
/// says so, so the scheduler places the second beside the first. Each stops on
/// its own with verified cleanup while the other keeps serving. An instance
/// restarted on an older generation than its sibling already reached on that
/// host (the I2 hazard) is admitted, not refused as stale.
// T24 T26 T27 T10 T33 T34
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_instances_of_one_deployment_co_reside_on_one_enrolled_host() {
    let cluster = Cluster::new();
    let server = cluster.start("server", &cluster.server_state, &cluster.server_config);
    let deadline = Instant::now() + Duration::from_secs(30);
    while !cluster
        .manage(&["list", "hosts", "--format", "json"])
        .status
        .success()
    {
        assert!(Instant::now() < deadline, "the server never answered");
        std::thread::sleep(Duration::from_millis(100));
    }
    let (roles, listed) = cluster.join_hosts();
    let a = listed["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["host_name"] == "i2-spark-a" || h["name"] == "i2-spark-a")
        .and_then(|h| h["host_id"].as_str())
        .unwrap()
        .to_owned();

    let mut pair = cluster.deployment("toy");
    pair["instances"] = json!(2);
    pair["placement"] = json!({"hosts": ["i2-spark-a"], "strategy": "pack"});
    let id = cluster.deploy(&pair, "pair.json");
    let status = cluster.ready_instances(&id, 2);
    for instance in status["instances"].as_array().unwrap() {
        assert_eq!(instance["host_id"], a.as_str(), "{status}");
    }
    let host_a = &cluster.hosts[0];
    assert_eq!(
        host_a.launches().len(),
        2,
        "two engines of one deployment on one host"
    );
    assert!(cluster.hosts[1].launches().is_empty());
    cluster.served(Duration::from_secs(30)).await;

    // Owner decision Q7: each instance stops on its own; the other serves.
    let engines = host_a.launches();
    let stop = |index: u32| {
        let stopped = cluster.manage_json(&[
            "stop",
            "instance",
            &format!("{id}/{index}"),
            "--format",
            "json",
        ]);
        cluster.succeeded(stopped["operation_id"].as_str().unwrap());
    };
    let start = |index: u32| {
        let started = cluster.manage_json(&[
            "start",
            "instance",
            &format!("{id}/{index}"),
            "--format",
            "json",
        ]);
        cluster.succeeded(started["operation_id"].as_str().unwrap());
    };
    stop(0);
    assert_eq!(engines.iter().filter(|pid| alive(**pid)).count(), 1);
    cluster.ready_instances(&id, 1);
    cluster.served(Duration::from_secs(10)).await;

    // Instance 1's stop reaches the host at a newer generation than the one
    // instance 0 keeps, and instance 0 then returns to the same host on its
    // older one: a host fencing per deployment would refuse it as stale.
    stop(1);
    assert!(engines.iter().all(|pid| !alive(*pid)));
    cluster.ready_instances(&id, 0);
    start(0);
    let status = cluster.ready_instances(&id, 1);
    let generation = |status: &Value, index: usize| -> i64 {
        status["instances"][index]["generation"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap()
    };
    assert!(
        generation(&status, 0) < generation(&status, 1),
        "instance 0 restarted below its sibling's generation: {status}"
    );
    start(1);
    let status = cluster.ready_instances(&id, 2);
    for instance in status["instances"].as_array().unwrap() {
        assert_eq!(instance["host_id"], a.as_str(), "{status}");
    }
    assert_eq!(host_a.launches().len(), 4);
    cluster.served(Duration::from_secs(10)).await;

    // The deployment's stop is one operation per instance; wait for both.
    cluster.manage_json(&["stop", "deployment", &id, "--format", "json"]);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let status = cluster.manage_json(&["status", "deployment", &id, "--format", "json"]);
        if status["observed_state"] == "stopped" {
            break;
        }
        assert!(Instant::now() < deadline, "never stopped: {status}");
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(host_a.launches().iter().all(|pid| !alive(*pid)));

    for role in roles {
        role.stop();
    }
    server.stop();
    let store = mllm_store::Store::open(&cluster.server_state.join("srv.sqlite3")).unwrap();
    assert!(store.resource_snapshot().unwrap().owners.is_empty());
}
