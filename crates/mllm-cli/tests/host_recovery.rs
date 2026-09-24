//! ADR 0016 (owner decision 2026-09-24): a revoked host recovers by
//! re-enrolling under its same identity, driven through the real `mllm`
//! binary: a server and one host role on this machine over mutual TLS, the
//! host running a fake vLLM executable behind its own private ingress.
//!
//! The administrator issues `invite host <name|id> --recover`; the host runs
//! `join host --recover` and gets a new certificate bound to the same host id,
//! while the old certificate stays revoked. On reconnect a Ready engine the
//! host still owns is re-proven by a fresh probe before dispatch reopens. A
//! host that lost its journal cannot re-prove anything: its engines stay
//! charged and closed until an operator stop settles them on gone evidence,
//! observed by identity.
//!
//! CPU and fake-engine tests are not qualification: passing this never shows
//! a native engine recipe works on either Spark (SPEC §18).

use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

mod support;
use support::process::{free_port, Guarded};

/// A vLLM stand-in: `serve <model> --port P --served-model-name R`, the key
/// only from `VLLM_API_KEY`, every `/v1` route keyed, one forked worker child
/// in the same process group. Each start appends its pid to `launches.log`.
const FAKE_VLLM: &str = r#"
import json, os, sys, time, http.server
args = sys.argv[1:]
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
        answer = "fake-vllm-answer"
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

const HOST: &str = "recover-spark";

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

fn stdout_json(out: &std::process::Output) -> Value {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

/// A running role; dropping it kills its process group (support::process).
struct Role(Guarded);
impl Role {
    fn stop(mut self) {
        self.0.signal(libc::SIGTERM);
        self.0.exit_within(Duration::from_secs(30), "the role");
    }
}

/// A server and one enrolled host on this machine.
struct Cluster {
    root: tempfile::TempDir,
    server_state: PathBuf,
    server_config: PathBuf,
    management: String,
    inference: String,
    host_state: PathBuf,
    host_config: PathBuf,
    identity_dir: PathBuf,
    engine: PathBuf,
}

impl Cluster {
    fn new() -> Self {
        let root = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.path().to_path_buf();
        let server_state = path.join("server");
        let server_config = path.join("server.yaml");
        stdout_json(&cli(
            &server_state,
            &["init", "server", "--output", server_config.to_str().unwrap()],
        ));
        let mut server: Value =
            serde_json::from_slice(&std::fs::read(&server_config).unwrap()).unwrap();
        let (mut inference, mut management) = (String::new(), String::new());
        for name in ["management", "inference", "bootstrap", "control"] {
            let address = format!("127.0.0.1:{}", free_port());
            server["listeners"][name]["bind"] = address.clone().into();
            if matches!(name, "bootstrap" | "control") {
                server["enrollment"][format!("{name}_address")] =
                    format!("https://{address}").into();
            }
            match name {
                "inference" => inference = address,
                "management" => management = address,
                _ => {}
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
        let host_state = path.join(HOST);
        let host_config = path.join(format!("{HOST}.yaml"));
        stdout_json(&cli(
            &host_state,
            &["init", "host", "--output", host_config.to_str().unwrap()],
        ));
        let engine = path.join("engine");
        std::fs::create_dir_all(&engine).unwrap();
        std::fs::write(
            engine.join("vllm"),
            format!("#!{}\n{FAKE_VLLM}", python3().display()),
        )
        .unwrap();
        std::fs::set_permissions(engine.join("vllm"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::os::unix::fs::symlink(python3(), engine.join("python3")).unwrap();
        // SPEC §13.3: the runtime directory the host trusts is its own and
        // writable by it alone.
        let runtime = path.join("runtime");
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
        let template: Value = serde_json::from_slice(&std::fs::read(&host_config).unwrap()).unwrap();
        let mut host = golden["input"]["host"].clone();
        host["name"] = HOST.into();
        host["state_dir"] = template["state_dir"].clone();
        host["identity_dir"] = template["identity_dir"].clone();
        host["runtime_dir"] = json!(runtime);
        host["model_store"]["path"] = json!(models);
        let port = support::process::free_ports(2, true)[0];
        host["resource_policy"]["endpoint_port_range"] = json!({"start": port, "end": port + 1});
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
        host["shutdown"] = json!({"drain_timeout": "10s"});
        std::fs::write(&host_config, serde_json::to_vec(&host).unwrap()).unwrap();
        let identity_dir = PathBuf::from(template["identity_dir"].as_str().unwrap());
        Self {
            root,
            server_state,
            server_config,
            management,
            inference,
            host_state,
            host_config,
            identity_dir,
            engine,
        }
    }

    fn launches(&self) -> Vec<i32> {
        std::fs::read_to_string(self.engine.join("launches.log"))
            .unwrap_or_default()
            .lines()
            .map(|line| line.parse().unwrap())
            .collect()
    }

    fn manage(&self, args: &[&str]) -> std::process::Output {
        let mut all = args.to_vec();
        all.extend(["--config", self.server_config.to_str().unwrap()]);
        cli(&self.server_state, &all)
    }

    fn manage_json(&self, args: &[&str]) -> Value {
        stdout_json(&self.manage(args))
    }

    fn join(&self, file: &Path, recover: bool) -> std::process::Output {
        let mut args = vec![
            "join",
            "host",
            "--join-file",
            file.to_str().unwrap(),
            "--config",
            self.host_config.to_str().unwrap(),
        ];
        if recover {
            args.push("--recover");
        }
        cli(&self.host_state, &args)
    }

    fn start(&self, role: &str, state: &Path, config: &Path) -> Role {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(state.with_extension("log"))
            .unwrap();
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

    fn start_server(&self) -> Role {
        let server = self.start("server", &self.server_state, &self.server_config);
        let deadline = Instant::now() + Duration::from_secs(30);
        while !self.manage(&["list", "hosts", "--output", "json"]).status.success() {
            assert!(Instant::now() < deadline, "the server never answered");
            std::thread::sleep(Duration::from_millis(100));
        }
        server
    }

    fn start_host(&self) -> Role {
        self.start("host", &self.host_state, &self.host_config)
    }

    fn hosts(&self) -> Value {
        self.manage_json(&["list", "hosts", "--output", "json"])
    }

    /// Wait until the one host is online and eligible.
    fn eligible(&self) -> Value {
        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            let listed = self.hosts();
            if listed["hosts"][0]["online"] == true && listed["hosts"][0]["eligible"] == true {
                return listed["hosts"][0].clone();
            }
            assert!(Instant::now() < deadline, "the host never became eligible: {listed}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// `invite host <host> [--recover]` into a new file; the command's output.
    fn invite(&self, host: &str, recover: bool, file: &str) -> (PathBuf, std::process::Output) {
        let path = self.root.path().join(file);
        let mut args = vec!["invite", "host", host, "--output", path.to_str().unwrap()];
        if recover {
            args.push("--recover");
        }
        let out = self.manage(&args);
        (path, out)
    }

    fn api_key(&self) -> String {
        let credentials: Value = serde_json::from_slice(
            &std::fs::read(self.server_state.join("identity/server-credentials.json")).unwrap(),
        )
        .unwrap();
        credentials["api_key"].as_str().unwrap().to_owned()
    }

    fn admin_token(&self) -> String {
        let credentials: Value = serde_json::from_slice(
            &std::fs::read(self.server_state.join("identity/server-credentials.json")).unwrap(),
        )
        .unwrap();
        credentials["admin_token"].as_str().unwrap().to_owned()
    }

    /// One chat request, bounded: a closed dispatch may hold a request queued,
    /// which reads here as not served.
    async fn chat(&self) -> Option<(u16, String)> {
        let response = reqwest::Client::new()
            .post(format!("http://{}/v1/chat/completions", self.inference))
            .bearer_auth(self.api_key())
            .timeout(Duration::from_secs(3))
            .json(&json!({"model": "toy", "messages": [{"role": "user", "content": "hello"}]}))
            .send()
            .await
            .ok()?;
        let status = response.status().as_u16();
        Some((status, response.text().await.unwrap_or_default()))
    }

    async fn served(&self, within: Duration) {
        let started = Instant::now();
        loop {
            if let Some((200, body)) = self.chat().await {
                assert!(body.contains("fake-vllm-answer"), "{body}");
                return;
            }
            assert!(started.elapsed() < within, "not served within {within:?}");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Dispatch stays closed for `during`: no request is answered.
    async fn not_served(&self, during: Duration) {
        let started = Instant::now();
        while started.elapsed() < during {
            let answer = self.chat().await;
            assert!(
                !matches!(answer, Some((200, _))),
                "a request was served while dispatch must be closed: {answer:?}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    fn store(&self) -> mllm_store::Store {
        mllm_store::Store::open(&self.server_state.join("srv.sqlite3")).unwrap()
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
            assert!(Instant::now() < deadline, "operation {operation} never succeeded");
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// Deploy a small restart-only deployment of the golden vLLM recipe on the
    /// host, serving `toy`, and wait for it to be Ready. Its id.
    fn deploy(&self) -> String {
        let golden: Value = serde_json::from_str(include_str!(
            "../../mllm-config/tests/fixtures/effective-vllm-golden.json"
        ))
        .unwrap();
        let mut deployment = golden["input"]["deployment"].clone();
        deployment["name"] = json!("toy");
        deployment["routes"] = json!(["toy"]);
        deployment["host"] = json!(HOST);
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

    /// The certificate fingerprint the host's identity file holds now.
    fn certificate_fingerprint(&self) -> String {
        let bundle: Value = serde_json::from_slice(
            &std::fs::read(self.identity_dir.join("host-identity.json")).unwrap(),
        )
        .unwrap();
        bundle["issued"]["fingerprint"].as_str().unwrap().to_owned()
    }

    fn events(&self, kind: &str) -> usize {
        self.store()
            .events_after(None, 1000)
            .unwrap()
            .events
            .iter()
            .filter(|event| event.kind == kind)
            .count()
    }

    /// Stop the deployment as an operator; the Stop succeeds only on gone
    /// evidence, and everything the deployment held is released.
    fn stop_deployment(&self, id: &str) {
        let stopped = self.manage_json(&["stop", "deployment", id, "--output", "json"]);
        self.succeeded(stopped["operation_id"].as_str().unwrap());
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        for pid in self.launches() {
            unsafe {
                libc::killpg(pid, libc::SIGKILL);
            }
        }
    }
}

fn now() -> i64 {
    mllm_protocol::now_unix_ms() / 1000
}

// T05 T06 T33 T34 (ADR 0016, SPEC §§4.1, 13.2, 13.3): the full recovery path
// through the product. A Ready fake engine runs on the enrolled host. Recovery
// of a host that is not revoked is refused. Revocation closes dispatch and a
// restarted host role is refused. A recovery invitation (by name) is redeemed
// only with `join host --recover`; the host keeps its state and journal and
// gets a new certificate for its same host id. It reconnects, the still-owned
// engine is re-proven by a fresh probe (not relaunched) and serves again. The
// old certificate stays refused, the invitation is single-use, both steps are
// journaled, and a final operator stop completes on gone evidence.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_revoked_host_recovers_its_identity_and_its_engine_is_reproven() {
    let cluster = Cluster::new();
    let server = cluster.start_server();
    let (file, out) = cluster.invite(HOST, false, "host.join");
    stdout_json(&out);
    stdout_json(&cluster.join(&file, false));
    let host = cluster.start_host();
    let listed = cluster.eligible();
    let host_id = listed["host_id"].as_str().unwrap().to_owned();
    let id = cluster.deploy();
    cluster.served(Duration::from_secs(30)).await;
    let engine = cluster.launches();
    assert_eq!(engine.len(), 1);

    // SPEC §4.1: recovery is for a revoked host only.
    let (_, out) = cluster.invite(HOST, true, "not-revoked.join");
    assert!(!out.status.success(), "an active host took a recovery invitation");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("host_not_revoked"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let old_fingerprint = cluster.certificate_fingerprint();
    cluster.manage_json(&["revoke", "host", HOST, "--output", "json"]);
    let listed = cluster.hosts();
    assert_eq!(listed["hosts"][0]["revoked"], true);
    // SPEC §13.3: dispatch to its engine is closed; the engine keeps running
    // and stays charged.
    cluster.not_served(Duration::from_secs(3)).await;
    assert!(engine.iter().all(|pid| alive(*pid)));
    assert!(!cluster.store().resource_snapshot().unwrap().owners.is_empty());
    // A restarted host role with its old identity is refused.
    host.stop();
    let host = cluster.start_host();
    for _ in 0..20 {
        assert_eq!(cluster.hosts()["hosts"][0]["online"], false);
        std::thread::sleep(Duration::from_millis(100));
    }
    // The operator stops the revoked host role before re-enrolling it.
    host.stop();

    // A recovery invitation names the revoked host's id; an ordinary join
    // refuses it; an ordinary invitation for the revoked name is refused.
    let (recovery, out) = cluster.invite(HOST, true, "recover.join");
    let invited = stdout_json(&out);
    assert_eq!(invited["recover_host_id"], host_id.as_str());
    assert_eq!(invited["host_name"], HOST);
    let (_, out) = cluster.invite(HOST, false, "collision.join");
    assert!(!out.status.success(), "a revoked name was reused for a new host");
    let out = cluster.join(&recovery, false);
    assert!(!out.status.success(), "an ordinary join redeemed a recovery invitation");
    let joined = stdout_json(&cluster.join(&recovery, true));
    assert_eq!(joined["host_id"], host_id.as_str(), "the same host id");
    assert_eq!(joined["recovered"], true);
    // An exact retry replays the same enrollment transaction.
    let replay = stdout_json(&cluster.join(&recovery, true));
    assert_eq!(replay["host_id"], host_id.as_str());
    let new_fingerprint = cluster.certificate_fingerprint();
    assert_ne!(new_fingerprint, old_fingerprint, "a new certificate");
    // Single use: the same invitation from fresh identity files is refused.
    let other_state = cluster.root.path().join("other-host");
    let other_config = cluster.root.path().join("other-host.yaml");
    stdout_json(&cli(
        &other_state,
        &["init", "host", "--output", other_config.to_str().unwrap()],
    ));
    let out = cli(
        &other_state,
        &["join", "host", "--join-file", recovery.to_str().unwrap(), "--config",
          other_config.to_str().unwrap(), "--recover"],
    );
    assert!(!out.status.success(), "a recovery invitation was redeemed twice");

    // The recovered host reconnects under its same id; its still-owned engine
    // is re-proven by a fresh probe and serves again, without a relaunch.
    let host = cluster.start_host();
    let listed = cluster.eligible();
    assert_eq!(listed["host_id"], host_id.as_str());
    assert_eq!(listed["revoked"], false);
    assert_eq!(cluster.hosts()["hosts"].as_array().unwrap().len(), 1, "no second host");
    cluster.served(Duration::from_secs(60)).await;
    assert_eq!(cluster.launches(), engine, "re-proven, not relaunched");
    assert!(engine.iter().all(|pid| alive(*pid)));

    // The old certificate stays revoked for ever; the new one authorizes.
    let store = cluster.store();
    assert!(store.certificate_host(&old_fingerprint, now()).is_err());
    assert_eq!(store.certificate_host(&new_fingerprint, now()).unwrap().host_id, host_id);
    drop(store);
    assert_eq!(cluster.events("host_revoked"), 1);
    assert_eq!(cluster.events("host_recovery_invited"), 1);
    assert_eq!(cluster.events("host_recovered"), 1);

    // An ordinary operator stop completes on the host's gone evidence.
    cluster.stop_deployment(&id);
    assert!(engine.iter().all(|pid| !alive(*pid)));
    host.stop();
    server.stop();
    let store = cluster.store();
    assert!(store.resource_snapshot().unwrap().owners.is_empty());
}

// T05 T06 T32 T33 (ADR 0016, SPEC §13.2, AGENTS.md: uncertainty retains
// accounting): a revoked host that also lost its identity files and journal
// recovers with fresh identity files under its same host id. An expired
// recovery invitation is refused first. The recovered host has no record of
// its engine, so nothing re-proves it: dispatch stays closed and the engine
// stays charged while it runs. An operator stop issued while the host is away
// stays pending; once the engine's processes are gone and the host is back,
// it settles on gone evidence the host observes by the recorded identities,
// and only then is everything released.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recovered_host_that_lost_its_journal_settles_engines_only_on_gone_evidence() {
    let cluster = Cluster::new();
    let server = cluster.start_server();
    let (file, out) = cluster.invite(HOST, false, "host.join");
    stdout_json(&out);
    stdout_json(&cluster.join(&file, false));
    let host = cluster.start_host();
    let host_id = cluster.eligible()["host_id"].as_str().unwrap().to_owned();
    let id = cluster.deploy();
    cluster.served(Duration::from_secs(30)).await;
    let engine = cluster.launches();
    assert_eq!(engine.len(), 1);

    cluster.manage_json(&["revoke", "host", &host_id, "--output", "json"]);
    host.stop();
    // The host loses its identity files and its whole state, journal included.
    std::fs::remove_file(cluster.identity_dir.join("host-identity.json")).unwrap();
    for entry in std::fs::read_dir(&cluster.host_state).unwrap() {
        let path = entry.unwrap().path();
        if path == cluster.identity_dir {
            continue;
        }
        if path.is_dir() {
            std::fs::remove_dir_all(&path).unwrap();
        } else {
            std::fs::remove_file(&path).unwrap();
        }
    }

    // T05: an expired recovery invitation is refused.
    let response = reqwest::Client::new()
        .post(format!("http://{}/management/v1/host-invitations", cluster.management))
        .bearer_auth(cluster.admin_token())
        .json(&json!({"host_name": host_id, "lifetime_seconds": 1, "recover": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 201);
    let expired: Value = response.json().await.unwrap();
    let expired_file = cluster.root.path().join("expired.join");
    private_file(&expired_file, &expired.to_string());
    tokio::time::sleep(Duration::from_millis(2100)).await;
    let out = cluster.join(&expired_file, true);
    assert!(!out.status.success(), "an expired recovery invitation was redeemed");

    let (recovery, out) = cluster.invite(&host_id, true, "recover.join");
    stdout_json(&out);
    let joined = stdout_json(&cluster.join(&recovery, true));
    assert_eq!(joined["host_id"], host_id.as_str(), "the same host id from fresh files");
    let host = cluster.start_host();
    let listed = cluster.eligible();
    assert_eq!(listed["host_id"], host_id.as_str());

    // The host has no record of the engine: nothing re-proves it. Dispatch
    // stays closed and the engine stays charged; nothing was signalled.
    cluster.not_served(Duration::from_secs(8)).await;
    assert!(engine.iter().all(|pid| alive(*pid)));
    assert!(!cluster.store().resource_snapshot().unwrap().owners.is_empty());
    assert_eq!(cluster.launches(), engine, "nothing relaunched");

    // The host role restarts (ordinary restart); while it is away an
    // operator stops the deployment, and the engine's processes end (an
    // operator killed them). The pending stop completes once the host is
    // back, on gone evidence the host observes by the recorded identities.
    host.stop();
    let stopped = cluster.manage_json(&["stop", "deployment", &id, "--output", "json"]);
    let operation = stopped["operation_id"].as_str().unwrap().to_owned();
    assert!(!cluster.store().resource_snapshot().unwrap().owners.is_empty());
    for pid in &engine {
        unsafe {
            libc::killpg(*pid, libc::SIGKILL);
        }
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while engine.iter().any(|pid| alive(*pid)) {
        assert!(Instant::now() < deadline, "the engine never exited");
        std::thread::sleep(Duration::from_millis(50));
    }
    let host = cluster.start_host();
    cluster.succeeded(&operation);
    host.stop();
    server.stop();
    let store = cluster.store();
    assert!(store.resource_snapshot().unwrap().owners.is_empty());
    assert!(store.pending_dispatches(&id).unwrap().is_empty());
}
