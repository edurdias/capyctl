//! Owner decision 2026-09-26: a client command run on the machine where a
//! role runs uses that role without `--config`: on the server machine the
//! server, on a standalone machine the standalone role, and on a host machine
//! the host (a command that needs the server says "this is a host; run this
//! on the server"). `--config` and `CAPYCTL_CONFIG` still win, in that order.
//! There are no saved contexts.
//!
//! Scripted management APIs only; the `capyctl` binary runs with a private HOME
//! (0700, under $HOME, removed after) per test. CPU tests; not qualification
//! of any engine.
mod support;

use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Arc;

const SERVER_TOKEN: &str = "server-admin-token-0123456789-abcdefghij";
const STANDALONE_TOKEN: &str = "standalone-admin-token-0123456789";

fn private_dir() -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("capyctl-local-role-")
        .tempdir_in(std::env::var_os("HOME").unwrap())
        .unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn mkdir(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn private_file(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
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

/// A scripted management API that answers only `token`, and names itself in
/// every answer so a test sees which one a command reached.
async fn api(token: &'static str, label: &'static str) -> std::net::SocketAddr {
    use axum::{extract::State, http::HeaderMap, http::StatusCode, routing, Json, Router};
    let state = Arc::new((token, label));
    let answer = |State(api): State<Arc<(&'static str, &'static str)>>, headers: HeaderMap| async move {
        let bearer = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        if bearer != Some(api.0) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"api_version": "1", "error": {"code": "unauthorized",
                    "message": "Unauthorized", "retryable": false}})),
            );
        }
        (
            StatusCode::OK,
            Json(json!({"operations": [], "deployments": [], "hosts": [
                {"host_id": api.1, "name": api.1, "online": true}]})),
        )
    };
    let app = Router::new()
        .route("/management/v1/snapshot", routing::get(answer))
        .route("/management/v1/hosts", routing::get(answer))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    address
}

/// A machine: a private HOME and a state root, with what a role records
/// there (a standalone role's or a server's credentials, the management
/// address it serves on, a host document).
struct Machine {
    root: tempfile::TempDir,
}

impl Machine {
    fn new() -> Self {
        let root = private_dir();
        mkdir(&root.path().join("home"));
        mkdir(&root.path().join("home/.config"));
        mkdir(&root.path().join("state"));
        mkdir(&root.path().join("state/identity"));
        Self { root }
    }
    fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }
    fn state(&self) -> PathBuf {
        self.root.path().join("state")
    }
    fn standalone(&self) -> &Self {
        private_file(
            &self.state().join("identity/credentials"),
            &format!("admin_token: {STANDALONE_TOKEN}\napi_key: k\n"),
        );
        self
    }
    fn server(&self) -> &Self {
        server_state(&self.state());
        self
    }
    fn recorded(&self, address: std::net::SocketAddr) -> &Self {
        record(&self.state(), address);
        self
    }
    /// The host document `init host` writes to its implicit place.
    fn host(&self) -> &Self {
        let out = self.capyctl(&["init", "host"], &[]);
        assert_eq!(out.status.code(), Some(0), "{}", text(&out));
        self
    }
    fn capyctl(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut command = support::capyctl();
        command
            .args(args)
            .env("HOME", self.home())
            .env("XDG_CONFIG_HOME", self.home().join(".config"))
            .env("CAPYCTL_STATE_DIR", self.state());
        for (key, value) in env {
            command.env(key, value);
        }
        command.output().unwrap()
    }
    async fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let (home, state) = (self.home(), self.state());
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let env: Vec<(String, String)> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        tokio::task::spawn_blocking(move || {
            let mut command = support::capyctl();
            command
                .args(&args)
                .env("HOME", &home)
                .env("XDG_CONFIG_HOME", home.join(".config"))
                .env("CAPYCTL_STATE_DIR", &state);
            for (key, value) in &env {
                command.env(key, value);
            }
            command.output().unwrap()
        })
        .await
        .unwrap()
    }
}

fn server_state(state: &Path) {
    mkdir(&state.join("identity"));
    private_file(
        &state.join("identity/server-credentials.json"),
        &json!({"version": 1, "admin_token": SERVER_TOKEN,
            "api_key": "server-api-key-0123456789-abcdefghijklmn"})
        .to_string(),
    );
}

/// A server document whose state is `state`, as `init server` writes it.
fn server_document(dir: &Path, state: &Path) -> PathBuf {
    let document = dir.join("server.yaml");
    std::fs::write(
        &document,
        json!({
            "schema_version": 1, "kind": "server", "name": "capyctl-server",
            "state_dir": state, "identity_dir": state.join("identity"),
            "listeners": {
                "management": {"bind": "127.0.0.1:7443", "authentication": "token"},
                "inference": {"bind": "127.0.0.1:8443", "authentication": "api_key"},
                "bootstrap": {"bind": "127.0.0.1:7444", "authentication": "server_tls"},
                "control": {"bind": "127.0.0.1:7445", "authentication": "mutual_tls"}},
            "enrollment": {"bootstrap_address": "https://127.0.0.1:7444",
                "control_address": "https://127.0.0.1:7445"}
        })
        .to_string(),
    )
    .unwrap();
    document
}

fn record(state: &Path, address: std::net::SocketAddr) {
    mkdir(&state.join("run"));
    private_file(
        &state.join("run/management-address"),
        &format!("{address}\n"),
    );
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn reached(output: &Output) -> String {
    let value: Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|_| panic!("not JSON: {}", text(output)));
    value["hosts"][0]["name"].as_str().unwrap_or("").to_owned()
}

const HOST_REFUSAL: &str = "This machine is a capyctl host; run this command on the server";

// T01: on a standalone machine a client command uses the standalone role with
// no flag; on the server machine, the server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_command_uses_the_server_or_standalone_role_on_this_machine() {
    let address = api(STANDALONE_TOKEN, "standalone").await;
    let machine = Machine::new();
    machine.standalone().recorded(address);
    let out = machine.run(&["list", "hosts", "--json"], &[]).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(reached(&out), "standalone");

    let address = api(SERVER_TOKEN, "server").await;
    let machine = Machine::new();
    machine.server().recorded(address);
    let out = machine.run(&["list", "hosts", "--json"], &[]).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(reached(&out), "server");
}

// T01: a server started with a named document whose state is elsewhere (as
// the packaged unit starts it) records the document; a client command on the
// machine finds that server without --config.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_named_with_a_document_is_found_without_config() {
    let address = api(SERVER_TOKEN, "server").await;
    let machine = Machine::new();
    let state = machine.root.path().join("var-lib-server");
    mkdir(&state);
    server_state(&state);
    record(&state, address);
    let document = server_document(machine.root.path(), &state);
    // What `start server --config <document>` records.
    mkdir(&machine.state().join("run"));
    private_file(
        &machine.state().join("run/server-document"),
        &format!("{}\n", document.display()),
    );
    let out = machine.run(&["list", "hosts", "--json"], &[]).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(reached(&out), "server");
}

// T01: on a host machine, a command that needs the server says plainly that
// this is a host and to run it on the server; the host's own commands
// (`config show`, `capyctl engine`) find the host without --config.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_a_host_machine_server_commands_say_to_run_them_on_the_server() {
    let machine = Machine::new();
    machine.host();
    for args in [
        &["list", "deployments"][..],
        &["list", "hosts"],
        &["status", "deployment", "chat"],
        &["deploy", "model", "--file", "chat.yaml"],
        &["revoke", "host", "gpu"],
    ] {
        let out = machine.run(args, &[]).await;
        let all = text(&out);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {all}");
        assert!(all.contains(HOST_REFUSAL), "{args:?}: {all}");
    }
    let out = machine.capyctl(&["config", "show", "--json"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let shown: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(shown["role"], "host", "{shown}");
    assert_eq!(
        shown["document"],
        machine.state().join("config/host.yaml").to_str().unwrap()
    );
}

// T01: a host started with a named document (`--config /etc/...`) records
// it; later commands on the machine find that document without --config, and
// `engine add` saves beside it, where the host reads its engines.
#[test]
fn a_host_named_with_a_document_is_found_without_config() {
    let machine = Machine::new();
    let named = machine.root.path().join("etc/host.yaml");
    mkdir(named.parent().unwrap());
    let out = machine.capyctl(&["init", "host", "--output", named.to_str().unwrap()], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    // What `start host --config <named>` records.
    mkdir(&machine.state().join("run"));
    private_file(
        &machine.state().join("run/host-document"),
        &format!("{}\n", named.display()),
    );
    let out = machine.capyctl(&["config", "show", "--json"], &[]);
    let shown: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(shown["role"], "host", "{shown}");
    assert_eq!(shown["document"], named.to_str().unwrap(), "{shown}");
    let env = vllm_env(&machine.root.path().join("v"));
    let out = machine.capyctl(&["engine", "add", env.to_str().unwrap()], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let beside = named.parent().unwrap().join("engines.yaml");
    assert!(
        std::fs::read_to_string(&beside)
            .unwrap_or_default()
            .contains("\"vllm\""),
        "{}: {}",
        beside.display(),
        text(&out)
    );
    let out = machine.capyctl(&["list", "deployments"], &[]);
    assert!(text(&out).contains(HOST_REFUSAL), "{}", text(&out));
}

// T01: with both a server and a standalone role under one state root, the one
// that answers is used; when neither answers, the command is refused naming
// both and the flag that chooses.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_two_roles_the_one_that_answers_is_used() {
    let address = api(SERVER_TOKEN, "server").await;
    let machine = Machine::new();
    machine.standalone().server().recorded(address);
    let out = machine.run(&["list", "hosts", "--json"], &[]).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(reached(&out), "server");

    let address = api("some-other-token", "other").await;
    let machine = Machine::new();
    machine.standalone().server().recorded(address);
    let out = machine.run(&["list", "hosts", "--json"], &[]).await;
    let all = text(&out);
    assert_eq!(out.status.code(), Some(2), "{all}");
    assert!(all.contains("server at"), "{all}");
    assert!(all.contains("standalone role at"), "{all}");
    assert!(all.contains("--config"), "{all}");
}

// T01: `--config` > `CAPYCTL_CONFIG` > the role on this machine.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_wins_over_capyctl_config_which_wins_over_detection() {
    let local = api(STANDALONE_TOKEN, "local").await;
    let named = api(SERVER_TOKEN, "named").await;
    let machine = Machine::new();
    machine.standalone().recorded(local);
    // Another server's state and document on this machine.
    let other = machine.root.path().join("other");
    mkdir(&other);
    server_state(&other);
    record(&other, named);
    let document = server_document(machine.root.path(), &other);
    let host = machine.root.path().join("host.yaml");
    std::fs::write(&host, "schema_version: 1\nkind: host\nname: gpu\n").unwrap();
    let (document, host) = (document.to_str().unwrap(), host.to_str().unwrap());
    let hosts = ["list", "hosts", "--json"];

    assert_eq!(reached(&machine.run(&hosts, &[]).await), "local");
    let out = machine.run(&hosts, &[("CAPYCTL_CONFIG", document)]).await;
    assert_eq!(reached(&out), "named", "{}", text(&out));
    let out = machine.run(&hosts, &[("CAPYCTL_CONFIG", host)]).await;
    assert!(text(&out).contains(HOST_REFUSAL), "{}", text(&out));
    let flagged = ["list", "hosts", "--json", "--config", document];
    let out = machine.run(&flagged, &[("CAPYCTL_CONFIG", host)]).await;
    assert_eq!(reached(&out), "named", "{}", text(&out));
}

// T01: saved contexts are gone: no `context` command, no `--context` flag,
// and help lists neither.
#[test]
fn there_are_no_context_commands() {
    let machine = Machine::new();
    let help = machine.capyctl(&["--help"], &[]);
    assert_eq!(help.status.code(), Some(0), "{}", text(&help));
    let help = text(&help);
    assert!(!help.to_lowercase().contains("context"), "{help}");
    for args in [
        &["context", "list"][..],
        &["context", "show"],
        &["list", "hosts", "--context", "lab"],
    ] {
        let out = machine.capyctl(args, &[]);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", text(&out));
    }
    let out = machine.capyctl(&["list", "hosts", "--help"], &[]);
    assert!(!text(&out).contains("--context"), "{}", text(&out));
}
