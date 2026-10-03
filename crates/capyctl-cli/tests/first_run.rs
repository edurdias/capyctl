//! First-run friction found by walking the user guides with the real binary
//! (2026-09-25), driven through the `capyctl` binary: `engine add` before any
//! role has started, a relative `--config`, and the first `deploy model
//! --activate` of a checkpoint whose digest is still being measured.
//!
//! Fake installations, a scripted role socket and a scripted management API
//! only. CPU tests; they are not qualification (SPEC §18).
mod support;

use capyctl_agent::control_socket::{ControlHandler, ControlRequest, ControlServer, SOCKET_NAME};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn private_dir() -> tempfile::TempDir {
    // Role custody rejects group-writable ancestors such as a shared /tmp.
    let dir = tempfile::Builder::new()
        .prefix("capyctl-first-run-")
        .tempdir_in(std::env::var_os("HOME").unwrap())
        .unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn mkdir(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn script(path: &Path, body: &str) {
    capyctl_config::test_support::write_executable(path, format!("#!/bin/sh\n{body}\n"), 0o755)
        .unwrap();
}

/// A vLLM 0.29.0 venv whose interpreter answers the capability probe.
fn vllm_env(root: &Path) -> PathBuf {
    let site = root.join("lib/python3.12/site-packages");
    std::fs::create_dir_all(site.join("vllm")).unwrap();
    std::fs::create_dir_all(site.join("vllm-0.29.0.dist-info")).unwrap();
    std::fs::write(
        site.join("vllm-0.29.0.dist-info/METADATA"),
        "Name: vllm\nVersion: 0.29.0\n",
    )
    .unwrap();
    std::fs::write(root.join("pyvenv.cfg"), "home = /usr/bin\n").unwrap();
    std::fs::create_dir_all(root.join("bin")).unwrap();
    script(&root.join("bin/vllm"), "echo 0.29.0");
    let report = json!({"schema": "capyctl/engine-capabilities/v1", "engine": "vllm",
        "capabilities": {"core": [], "deep_park": [], "metrics": []}});
    script(&root.join("bin/python3"), &format!("echo '{report}'"));
    root.to_path_buf()
}

/// `capyctl <args>` with a clean environment: `HOME` and the state directory
/// under `root`, run from `cwd`, plus `extra`.
fn capyctl(root: &Path, cwd: &Path, args: &[&str], extra: &[(&str, &str)]) -> Output {
    let mut command = support::capyctl();
    command
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", root.join("home"))
        .env("CAPYCTL_STATE_DIR", root.join("state"));
    for (key, value) in extra {
        command.env(key, value);
    }
    command.output().unwrap()
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

// T07 (ADR 0018 §3; owner decision 2026-09-25): the first `engine add` on a
// machine where no role runs yet (standalone refuses to start with no engine)
// saves the profile and exits 0 with a notice naming the file and what to run,
// not `agent_unreachable`.
#[test]
fn engine_add_with_no_role_running_saves_and_succeeds() {
    let root = private_dir();
    mkdir(&root.path().join("home"));
    let env = vllm_env(&root.path().join("v"));
    let output = capyctl(
        root.path(),
        root.path(),
        &["engine", "add", env.to_str().unwrap(), "--json"],
        &[],
    );
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    let engines = root.path().join("home/.config/capyctl/engines.yaml");
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["published"], "role_not_running", "{}", text(&output));
    // The notice is part of the result: with `--json` it is the `notice` field.
    let notice = result["notice"].as_str().unwrap_or_default();
    assert!(
        notice.contains(&format!("saved to {}", engines.display()))
            && notice.contains("start capyctl")
            && notice.contains("capyctl start standalone"),
        "{}",
        text(&output)
    );
    assert!(
        !text(&output).contains("agent_unreachable"),
        "{}",
        text(&output)
    );
    assert!(std::fs::read_to_string(&engines)
        .unwrap()
        .contains("\"vllm\""));
}

// T07 T37: a role that is there but does not answer is still a fault, exit 22
// (`agent_unreachable`); only "no role running" is the quiet first run.
#[test]
fn engine_add_with_a_role_that_does_not_answer_is_agent_unreachable() {
    let root = private_dir();
    mkdir(&root.path().join("home"));
    let state = root.path().join("state");
    mkdir(&state);
    let env = vllm_env(&root.path().join("v"));
    let listener = std::os::unix::net::UnixListener::bind(state.join(SOCKET_NAME)).unwrap();
    let role = std::thread::spawn(move || {
        use std::io::BufRead;
        let (stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        std::io::BufReader::new(&stream)
            .read_line(&mut line)
            .unwrap();
        drop(stream);
    });
    let output = capyctl(
        root.path(),
        root.path(),
        &["engine", "add", env.to_str().unwrap()],
        &[],
    );
    role.join().unwrap();
    assert_eq!(output.status.code(), Some(22), "{}", text(&output));
    assert!(
        text(&output).contains("agent_unreachable"),
        "{}",
        text(&output)
    );
}

// T07 (found live 2026-10-03): a state directory whose control socket path
// is longer than a Unix socket allows can never run a role, so `engine add`
// refuses it before writing anything, with what to do.
#[test]
fn engine_add_with_a_state_dir_too_long_for_its_socket_writes_nothing() {
    let root = private_dir();
    mkdir(&root.path().join("home"));
    let env = vllm_env(&root.path().join("v"));
    let state = root.path().join("s".repeat(110));
    let output = capyctl(
        root.path(),
        root.path(),
        &[
            "engine",
            "add",
            env.to_str().unwrap(),
            "--state-dir",
            state.to_str().unwrap(),
        ],
        &[],
    );
    assert_eq!(output.status.code(), Some(2), "{}", text(&output));
    assert!(
        text(&output).contains("socket path limit"),
        "{}",
        text(&output)
    );
    assert!(
        text(&output).contains("shorter --state-dir"),
        "{}",
        text(&output)
    );
    assert!(
        !root
            .path()
            .join("home/.config/capyctl/engines.yaml")
            .exists(),
        "{}",
        text(&output)
    );
}

struct Role(AtomicUsize);
#[async_trait::async_trait]
impl ControlHandler for Role {
    async fn handle(&self, request: ControlRequest) -> Value {
        assert_eq!(request, ControlRequest::Add);
        self.0.fetch_add(1, Ordering::SeqCst);
        json!({"ok": true, "published": "published"})
    }
}

/// A host document in `dir` whose private state directory holds a scripted
/// role socket that publishes every `add`.
async fn host_with_role(root: &Path) -> (PathBuf, Arc<Role>, tokio::sync::watch::Sender<bool>) {
    let state = root.join("s");
    mkdir(&state);
    let dir = root.join("etc");
    mkdir(&dir);
    std::fs::write(
        dir.join("host.yaml"),
        capyctl_config::remote_roles::HostConfig::template(&state),
    )
    .unwrap();
    let server = ControlServer::bind(&state.join(SOCKET_NAME)).unwrap();
    let role = Arc::new(Role(AtomicUsize::new(0)));
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    tokio::spawn(server.serve(role.clone(), unsafe { libc::geteuid() }, shutdown));
    (dir, role, stop)
}

// T01 T07 (ADR 0018 §2): `engine add --config host.yaml` with a path relative
// to the working directory saves the profile beside the document and asks the
// running role to publish it (found 2026-09-25: it saved, then failed to sync
// the empty parent directory and never asked the role).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_add_with_a_relative_config_publishes() {
    let root = private_dir();
    mkdir(&root.path().join("home"));
    let env = vllm_env(&root.path().join("v"));
    let (dir, role, _stop) = host_with_role(root.path()).await;
    let (home, cwd, venv) = (root.path().to_owned(), dir.clone(), env.clone());
    let output = tokio::task::spawn_blocking(move || {
        capyctl(
            &home,
            &cwd,
            &[
                "engine",
                "add",
                venv.to_str().unwrap(),
                "--config",
                "host.yaml",
                "--json",
            ],
            &[],
        )
    })
    .await
    .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["published"], "published", "{}", text(&output));
    assert_eq!(
        result["engines_file"],
        dir.join("engines.yaml").to_string_lossy().as_ref(),
        "the engines file is named absolutely"
    );
    assert_eq!(role.0.load(Ordering::SeqCst), 1, "the role was asked");
    assert!(dir.join("engines.yaml").is_file());
}

// T01 (ADR 0018 §2): `$CAPYCTL_CONFIG` relative to the working directory is
// resolved the same way.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_add_with_a_relative_capyctl_config_publishes() {
    let root = private_dir();
    mkdir(&root.path().join("home"));
    let env = vllm_env(&root.path().join("v"));
    let (dir, role, _stop) = host_with_role(root.path()).await;
    let (home, cwd, venv) = (root.path().to_owned(), dir.clone(), env.clone());
    let output = tokio::task::spawn_blocking(move || {
        capyctl(
            &home,
            &cwd,
            &["engine", "add", venv.to_str().unwrap(), "--json"],
            &[("CAPYCTL_CONFIG", "host.yaml")],
        )
    })
    .await
    .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["published"], "published", "{}", text(&output));
    assert_eq!(role.0.load(Ordering::SeqCst), 1, "the role was asked");
}

/// A scripted management API: a deploy is accepted with its checkpoint
/// digest pending; the digest is recorded after `measured_after` snapshot
/// reads (never, when `None`); a start is refused `checkpoint_digest_pending`
/// while it is pending, as the store does (ADR 0014 §7).
struct Management {
    reads: AtomicUsize,
    refused_starts: AtomicUsize,
    starts: AtomicUsize,
    measured_after: Option<usize>,
    /// ADR 0008: the deployment declares a remote source, downloading until
    /// the digest would be recorded (the digest itself is then non-provisional).
    source: bool,
}

impl Management {
    fn pending(&self) -> bool {
        self.measured_after
            .is_none_or(|after| self.reads.load(Ordering::SeqCst) < after)
    }
}

const DEPLOYMENT_ID: &str = "01K00000000000000000000001";
const OPERATION_ID: &str = "01K00000000000000000000002";

async fn management(state: Arc<Management>, initialize_ms: i64) -> std::net::SocketAddr {
    use axum::{extract::State, http::StatusCode, routing, Json, Router};
    let app = Router::new()
        .route(
            "/management/v1/deployments",
            routing::post(|| async {
                (
                    StatusCode::ACCEPTED,
                    Json(json!({"api_version": "1", "operation_id": OPERATION_ID,
                        "deployment_id": DEPLOYMENT_ID, "revision": "1", "joined": false,
                        "checkpoint_digest": "pending"})),
                )
            }),
        )
        .route(
            "/management/v1/snapshot",
            routing::get(move |State(m): State<Arc<Management>>| async move {
                m.reads.fetch_add(1, Ordering::SeqCst);
                let pending = m.pending();
                let mut item = json!({
                    "id": DEPLOYMENT_ID, "name": "first-model", "revision": "1",
                    "timeouts": {"initialize_ms": initialize_ms, "request_deadline_ms": 600_000},
                    "checkpoint_digest": {"state": if pending { "pending" } else { "recorded" },
                        "host_id": "h", "provisional": !m.source},
                });
                if m.source {
                    item["model_sources"] = json!([{"host_id": "h",
                        "source_key": "sources/huggingface/o--m@0123456789abcdef0123456789abcdef01234567",
                        "state": if pending { "downloading" } else { "verified" },
                        "bytes_done": 10, "bytes_total": 100}]);
                }
                Json(json!({"operations": [], "deployments": [item]}))
            }),
        )
        .route(
            &format!("/management/v1/deployments/{DEPLOYMENT_ID}/actions"),
            routing::post(|State(m): State<Arc<Management>>| async move {
                if m.pending() {
                    m.refused_starts.fetch_add(1, Ordering::SeqCst);
                    let code = if m.source { "model_source_pending" } else { "checkpoint_digest_pending" };
                    return (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(json!({"api_version": "1", "error": {"code": code,
                            "message": "Still pending; retry shortly",
                            "retryable": true, "operation_id": null, "details": {}}})),
                    );
                }
                m.starts.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::ACCEPTED,
                    Json(json!({"api_version": "1", "operation_id": "01K00000000000000000000003",
                        "deployment_id": DEPLOYMENT_ID})),
                )
            }),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    address
}

/// A state directory holding standalone management credentials, and a
/// deployment file.
fn deploy_fixture(root: &Path) -> PathBuf {
    mkdir(&root.join("home"));
    mkdir(&root.join("state"));
    mkdir(&root.join("state/identity"));
    std::fs::write(
        root.join("state/identity/credentials"),
        "admin_token: first-run-token\n",
    )
    .unwrap();
    let golden: Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    let mut deployment = golden["input"]["deployment"].clone();
    deployment["name"] = json!("first-model");
    let path = root.join("deployment.json");
    std::fs::write(&path, deployment.to_string()).unwrap();
    path
}

async fn deploy(root: &Path, address: std::net::SocketAddr, args: &[&str]) -> Output {
    let (home, file) = (root.to_owned(), deploy_fixture(root));
    let args: Vec<String> = ["deploy", "model", "--file", file.to_str().unwrap()]
        .iter()
        .map(|s| s.to_string())
        .chain(args.iter().map(|s| s.to_string()))
        .collect();
    tokio::task::spawn_blocking(move || {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let address = address.to_string();
        capyctl(
            &home,
            &home,
            &args,
            &[(capyctl_cli::roles::MANAGEMENT_ADDR_ENV, &address)],
        )
    })
    .await
    .unwrap()
}

// T08 (ADR 0014 §7; SPEC §6.4): the first `deploy model --activate` of a new
// checkpoint waits for its digest to be measured, then starts it: one
// command, exit 0, and the start is never sent while the digest is pending.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deploy_activate_waits_for_the_checkpoint_digest() {
    let root = private_dir();
    let state = Arc::new(Management {
        reads: AtomicUsize::new(0),
        refused_starts: AtomicUsize::new(0),
        starts: AtomicUsize::new(0),
        measured_after: Some(3),
        source: false,
    });
    let address = management(state.clone(), 60_000).await;
    let output = deploy(root.path(), address, &["--activate", "--json"]).await;
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["operation_id"], "01K00000000000000000000003");
    assert_eq!(state.starts.load(Ordering::SeqCst), 1);
    assert_eq!(state.refused_starts.load(Ordering::SeqCst), 0);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("checkpoint digest of first-model"),
        "{}",
        text(&output)
    );
}

// T08: the wait is bounded by the start's Initialize window; when it expires
// nothing is started and the error says what to run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deploy_activate_gives_up_after_the_initialize_window() {
    let root = private_dir();
    let state = Arc::new(Management {
        reads: AtomicUsize::new(0),
        refused_starts: AtomicUsize::new(0),
        starts: AtomicUsize::new(0),
        measured_after: None,
        source: false,
    });
    let address = management(state.clone(), 1_000).await;
    let output = deploy(root.path(), address, &["--activate"]).await;
    assert_ne!(output.status.code(), Some(0), "{}", text(&output));
    let all = text(&output);
    assert!(all.contains("activation_timeout"), "{all}");
    assert!(all.contains("checkpoint_digest_pending"), "{all}");
    assert!(
        all.contains("capyctl start deployment first-model --wait"),
        "{all}"
    );
    assert_eq!(state.starts.load(Ordering::SeqCst), 0);
    assert_eq!(state.refused_starts.load(Ordering::SeqCst), 0);
}

// T08: without `--activate` the deploy stays asynchronous and says what
// starts it once the digest is measured.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deploy_without_activate_says_what_to_run() {
    let root = private_dir();
    let state = Arc::new(Management {
        reads: AtomicUsize::new(0),
        refused_starts: AtomicUsize::new(0),
        starts: AtomicUsize::new(0),
        measured_after: None,
        source: false,
    });
    let address = management(state.clone(), 60_000).await;
    let output = deploy(root.path(), address, &[]).await;
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("`capyctl start deployment first-model --wait`"),
        "{}",
        text(&output)
    );
    assert_eq!(state.starts.load(Ordering::SeqCst), 0);
}

// T08 T14 (ADR 0008, owner decision 2026-09-25): `deploy model --activate`
// of a declared remote source waits while the host downloads it, then starts
// it; the start is never sent while the source is pending (found live: it was
// refused `model_source_pending` at once).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deploy_activate_waits_for_the_model_source() {
    let root = private_dir();
    let state = Arc::new(Management {
        reads: AtomicUsize::new(0),
        refused_starts: AtomicUsize::new(0),
        starts: AtomicUsize::new(0),
        measured_after: Some(3),
        source: true,
    });
    let address = management(state.clone(), 60_000).await;
    let output = deploy(root.path(), address, &["--activate"]).await;
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    assert_eq!(state.starts.load(Ordering::SeqCst), 1);
    assert_eq!(state.refused_starts.load(Ordering::SeqCst), 0);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("model source of first-model"),
        "{}",
        text(&output)
    );
}

// T08 T14: the source wait is bounded by the same Initialize window; when it
// expires nothing is started and the error says what to run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deploy_activate_gives_up_on_a_source_after_the_initialize_window() {
    let root = private_dir();
    let state = Arc::new(Management {
        reads: AtomicUsize::new(0),
        refused_starts: AtomicUsize::new(0),
        starts: AtomicUsize::new(0),
        measured_after: None,
        source: true,
    });
    let address = management(state.clone(), 1_000).await;
    let output = deploy(root.path(), address, &["--activate"]).await;
    assert_ne!(output.status.code(), Some(0), "{}", text(&output));
    let all = text(&output);
    assert!(all.contains("activation_timeout"), "{all}");
    assert!(all.contains("model_source_pending"), "{all}");
    assert!(
        all.contains("capyctl start deployment first-model --wait"),
        "{all}"
    );
    assert_eq!(state.starts.load(Ordering::SeqCst), 0);
    assert_eq!(state.refused_starts.load(Ordering::SeqCst), 0);
}

// T14 (standalone is a server plus one host, shared defaults): `init host`
// writes a document that validates as written, and its models directory is
// the shared default (`~/models`, downloads in `~/models/sources`, allowed),
// not a directory under the state root.
#[test]
fn init_host_writes_a_document_that_validates_with_the_shared_defaults() {
    let root = private_dir();
    mkdir(&root.path().join("home"));
    let output = capyctl(root.path(), root.path(), &["init", "host"], &[]);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    let config = root.path().join("state/config/host.yaml");
    let document: Value =
        capyctl_config::parse_document(&std::fs::read_to_string(&config).unwrap()).unwrap();
    assert!(
        document["model_store"]["path"]
            .as_str()
            .is_none_or(|path| !path.starts_with(root.path().join("state").to_str().unwrap())),
        "{document}"
    );
    let output = capyctl(
        root.path(),
        root.path(),
        &["validate", "config", "--file", config.to_str().unwrap()],
        &[],
    );
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    let output = capyctl(
        root.path(),
        root.path(),
        &["config", "show", "--role", "host", "--json"],
        &[],
    );
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    let shown: Value = serde_json::from_slice(&output.stdout).unwrap();
    let setting = |path: &str| {
        shown["settings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["path"] == path)
            .unwrap_or_else(|| panic!("{path} in {shown}"))
            .clone()
    };
    let models = root.path().join("home/models");
    assert_eq!(
        setting("model_store.path")["value"],
        models.to_str().unwrap()
    );
    assert_eq!(setting("model_store.path")["source"], "default");
    assert_eq!(setting("model_sources.huggingface")["value"], "allowed");
    assert_eq!(setting("model_sources.http")["value"], "allowed");
}

/// A scripted management API for a deployment whose stop is still settling:
/// the snapshot shows it `stopping` for `stopped_after` reads, and a start is
/// refused `runtime_retained` meanwhile, as the store does.
struct Stopping {
    reads: AtomicUsize,
    refused_starts: AtomicUsize,
    starts: AtomicUsize,
    stopped_after: usize,
    /// Starts refused `still_stopping` (503, retryable) while the snapshot
    /// already reads stopped: the shape after a revision redeploy, whose old
    /// engine is still being confirmed gone (found live 2026-10-03).
    still_stopping_refusals: usize,
}

const START_ID: &str = "01K00000000000000000000004";

async fn stopping_management(state: Arc<Stopping>) -> std::net::SocketAddr {
    use axum::{extract::State, http::StatusCode, routing, Json, Router};
    let still = |m: &Stopping| m.reads.load(Ordering::SeqCst) < m.stopped_after;
    let app = Router::new()
        .route(
            "/management/v1/snapshot",
            routing::get(move |State(m): State<Arc<Stopping>>| async move {
                m.reads.fetch_add(1, Ordering::SeqCst);
                let state = if still(&m) { "stopping" } else { "stopped" };
                let operations = if m.starts.load(Ordering::SeqCst) > 0 {
                    json!([{"id": START_ID, "state": "succeeded", "kind": "initialize"}])
                } else {
                    json!([])
                };
                Json(json!({"operations": operations, "deployments": [{
                    "id": DEPLOYMENT_ID, "name": "first-model", "revision": "1",
                    "observed_state": state, "desired_state": "stopped",
                    "timeouts": {"initialize_ms": 60_000, "request_deadline_ms": 600_000},
                    "checkpoint_digest": {"state": "recorded", "host_id": "h", "provisional": false},
                }]}))
            }),
        )
        .route(
            &format!("/management/v1/deployments/{DEPLOYMENT_ID}/actions"),
            routing::post(move |State(m): State<Arc<Stopping>>| async move {
                if m.refused_starts.load(Ordering::SeqCst) < m.still_stopping_refusals {
                    m.refused_starts.fetch_add(1, Ordering::SeqCst);
                    return (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(json!({"api_version": "1", "error": {"code": "still_stopping",
                            "message": "A stop is still being confirmed",
                            "retryable": true, "operation_id": null, "details": {}}})),
                    );
                }
                if still(&m) {
                    m.refused_starts.fetch_add(1, Ordering::SeqCst);
                    return (
                        StatusCode::CONFLICT,
                        Json(json!({"api_version": "1", "error": {"code": "runtime_retained",
                            "message": "Runtime ownership requires verified cleanup",
                            "retryable": false, "operation_id": null, "details": {}}})),
                    );
                }
                m.starts.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::ACCEPTED,
                    Json(json!({"api_version": "1", "operation_id": START_ID,
                        "deployment_id": DEPLOYMENT_ID})),
                )
            }),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    address
}

async fn start(root: &Path, address: std::net::SocketAddr, extra: &[&str]) -> Output {
    deploy_fixture(root);
    let home = root.to_owned();
    let args: Vec<String> = ["start", "deployment", "first-model"]
        .iter()
        .chain(extra)
        .map(|s| s.to_string())
        .collect();
    tokio::task::spawn_blocking(move || {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let address = address.to_string();
        capyctl(
            &home,
            &home,
            &args,
            &[(capyctl_cli::roles::MANAGEMENT_ADDR_ENV, &address)],
        )
    })
    .await
    .unwrap()
}

// T08 (SPEC §6.4): `start` right after `stop`, while the stop is still
// settling, says so in plain words and exits with its own code, not the
// invalid-configuration exit 2.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_while_the_stop_settles_says_still_stopping() {
    let root = private_dir();
    let state = Arc::new(Stopping {
        reads: AtomicUsize::new(0),
        refused_starts: AtomicUsize::new(0),
        starts: AtomicUsize::new(0),
        stopped_after: usize::MAX,
        still_stopping_refusals: 0,
    });
    let address = stopping_management(state.clone()).await;
    let output = start(root.path(), address, &[]).await;
    let all = text(&output);
    assert_eq!(output.status.code(), Some(25), "{all}");
    assert!(all.contains("still_stopping"), "{all}");
    assert!(all.contains("first-model is still stopping"), "{all}");
    assert!(
        all.contains("capyctl start deployment first-model --wait"),
        "{all}"
    );
    assert_eq!(state.starts.load(Ordering::SeqCst), 0);
}

// T08 (SPEC §6.4): `start --wait` right after `stop` waits for the stop to
// settle, then starts: one command, exit 0, never refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_wait_waits_for_the_stop_to_settle() {
    let root = private_dir();
    let state = Arc::new(Stopping {
        reads: AtomicUsize::new(0),
        refused_starts: AtomicUsize::new(0),
        starts: AtomicUsize::new(0),
        stopped_after: 3,
        still_stopping_refusals: 0,
    });
    let address = stopping_management(state.clone()).await;
    let output = start(root.path(), address, &["--wait"]).await;
    let all = text(&output);
    assert_eq!(output.status.code(), Some(0), "{all}");
    assert!(all.contains("Waiting for the stop of first-model"), "{all}");
    assert_eq!(state.starts.load(Ordering::SeqCst), 1);
    assert_eq!(state.refused_starts.load(Ordering::SeqCst), 0);
}

// T08 (SPEC §6.4, found live 2026-10-03): right after a revision redeploy the
// snapshot already reads stopped while the old engine's stop is still being
// confirmed, and the start is refused `still_stopping`. `--wait` retries it
// until the stop settles, then starts; without `--wait` the refusal stands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_wait_retries_a_still_stopping_refusal() {
    let root = private_dir();
    let state = Arc::new(Stopping {
        reads: AtomicUsize::new(0),
        refused_starts: AtomicUsize::new(0),
        starts: AtomicUsize::new(0),
        stopped_after: 0,
        still_stopping_refusals: 2,
    });
    let address = stopping_management(state.clone()).await;
    let output = start(root.path(), address, &["--wait"]).await;
    let all = text(&output);
    assert_eq!(output.status.code(), Some(0), "{all}");
    assert!(all.contains("Waiting for the stop of first-model"), "{all}");
    assert_eq!(state.refused_starts.load(Ordering::SeqCst), 2);
    assert_eq!(state.starts.load(Ordering::SeqCst), 1);
}

/// A management API whose deployment's checkpoint digest is pending with a
/// closed diagnostic the host reported.
async fn unmeasurable_management(
    diagnostic: &'static str,
    starts: Arc<AtomicUsize>,
) -> std::net::SocketAddr {
    use axum::{extract::State, http::StatusCode, routing, Json, Router};
    let app = Router::new()
        .route(
            "/management/v1/snapshot",
            routing::get(move || async move {
                Json(json!({"operations": [], "deployments": [{
                    "id": DEPLOYMENT_ID, "name": "first-model", "revision": "1",
                    "observed_state": "stopped", "desired_state": "stopped",
                    "timeouts": {"initialize_ms": 60_000, "request_deadline_ms": 600_000},
                    "checkpoint_digest": {"state": "pending", "host_id": "h",
                        "provisional": true, "diagnostic": diagnostic},
                }]}))
            }),
        )
        .route(
            &format!("/management/v1/deployments/{DEPLOYMENT_ID}/actions"),
            routing::post(move |State(starts): State<Arc<AtomicUsize>>| async move {
                starts.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::ACCEPTED,
                    Json(json!({"api_version": "1",
                    "operation_id": START_ID, "deployment_id": DEPLOYMENT_ID})),
                )
            }),
        )
        .with_state(starts);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    address
}

// T16 (ADR 0014 §7, found live 2026-10-03): a checkpoint the host cannot
// measure (here a path it refused) ends `start --wait` at once with the reason
// and nothing started, instead of waiting out the Initialize window.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_wait_ends_at_once_on_a_checkpoint_that_cannot_be_measured() {
    let root = private_dir();
    let starts = Arc::new(AtomicUsize::new(0));
    let address = unmeasurable_management("invalid_root", starts.clone()).await;
    let began = std::time::Instant::now();
    let output = start(root.path(), address, &["--wait"]).await;
    let all = text(&output);
    assert!(
        began.elapsed() < std::time::Duration::from_secs(20),
        "{all}"
    );
    assert_eq!(output.status.code(), Some(2), "{all}");
    assert!(
        all.contains("could not be measured (invalid_root)"),
        "{all}"
    );
    assert!(all.contains("first-model"), "{all}");
    assert_eq!(starts.load(Ordering::SeqCst), 0);
}
