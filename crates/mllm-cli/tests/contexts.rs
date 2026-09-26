//! Owner decision 2026-09-26: client commands find their management API
//! without `--config` every time. Auto-detection of the role running on this
//! machine (server or standalone) from what it records under the state root,
//! saved contexts (`mllm context add|use|list|remove|show`) and their
//! precedence: `--context`/`--config` > `MLLM_CONTEXT` > the current context
//! > auto-detection.
//!
//! Scripted management APIs only; the `mllm` binary runs with a private
//! HOME per test. CPU tests; not qualification of any engine.
mod support;

use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const SERVER_TOKEN: &str = "server-admin-token-0123456789";
const STANDALONE_TOKEN: &str = "standalone-admin-token-0123456789";

fn private_dir() -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("mllm-contexts-")
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

/// A scripted management API that answers only `token`, and names itself in
/// every answer so a test sees which one a command reached.
struct Api {
    token: &'static str,
    label: &'static str,
    calls: AtomicUsize,
}

async fn api(token: &'static str, label: &'static str) -> (std::net::SocketAddr, Arc<Api>) {
    use axum::{extract::State, http::HeaderMap, http::StatusCode, routing, Json, Router};
    let state = Arc::new(Api {
        token,
        label,
        calls: AtomicUsize::new(0),
    });
    let answer = |State(api): State<Arc<Api>>, headers: HeaderMap| async move {
        api.calls.fetch_add(1, Ordering::SeqCst);
        let bearer = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        if bearer != Some(api.token) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"api_version": "1", "error": {"code": "unauthorized",
                    "message": "Unauthorized", "retryable": false}})),
            );
        }
        (
            StatusCode::OK,
            Json(json!({"operations": [], "deployments": [], "hosts": [
                {"host_id": api.label, "name": api.label, "online": true}]})),
        )
    };
    let app = Router::new()
        .route("/management/v1/snapshot", routing::get(answer))
        .route("/management/v1/hosts", routing::get(answer))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (address, state)
}

/// A machine: a private HOME and a state root, with a standalone role's
/// and/or a server's credentials recorded, and the management address the
/// last role recorded.
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
        private_file(
            &self.state().join("identity/server-credentials.json"),
            &json!({"version": 1, "admin_token": SERVER_TOKEN, "api_key": "k"}).to_string(),
        );
        self
    }
    fn recorded(&self, address: std::net::SocketAddr) -> &Self {
        mkdir(&self.state().join("run"));
        private_file(
            &self.state().join("run/management-address"),
            &format!("{address}\n"),
        );
        self
    }
    fn mllm(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut command = support::mllm();
        command
            .args(args)
            .env("HOME", self.home())
            .env("XDG_CONFIG_HOME", self.home().join(".config"))
            .env("MLLM_STATE_DIR", self.state());
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
            let mut command = support::mllm();
            command
                .args(&args)
                .env("HOME", &home)
                .env("XDG_CONFIG_HOME", home.join(".config"))
                .env("MLLM_STATE_DIR", &state);
            for (key, value) in &env {
                command.env(key, value);
            }
            command.output().unwrap()
        })
        .await
        .unwrap()
    }
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

// T01: with only a standalone role recorded, a client command uses it with no
// flag; with only a server recorded, the server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_command_finds_the_role_running_on_this_machine() {
    let (address, _) = api(STANDALONE_TOKEN, "standalone").await;
    let machine = Machine::new();
    machine.standalone().recorded(address);
    let out = machine.run(&["list", "hosts", "--json"], &[]).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(reached(&out), "standalone");

    let (address, _) = api(SERVER_TOKEN, "server").await;
    let machine = Machine::new();
    machine.server().recorded(address);
    let out = machine.run(&["list", "hosts", "--json"], &[]).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(reached(&out), "server");
    let out = machine.run(&["context", "show"], &[]).await;
    let shown: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(shown["source"], "detected", "{shown}");
    assert_eq!(shown["role"], "server", "{shown}");
    assert_eq!(shown["server"], address.to_string(), "{shown}");
}

// T01: with both a server and a standalone role recorded, the one that
// answers is used; when neither answers, the command is refused naming both
// and how to pick one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_both_roles_recorded_the_one_that_answers_is_used() {
    let (address, _) = api(SERVER_TOKEN, "server").await;
    let machine = Machine::new();
    machine.standalone().server().recorded(address);
    let out = machine.run(&["list", "hosts", "--json"], &[]).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(reached(&out), "server");

    let (address, _) = api("some-other-token", "other").await;
    let machine = Machine::new();
    machine.standalone().server().recorded(address);
    let out = machine.run(&["list", "hosts", "--json"], &[]).await;
    let all = text(&out);
    assert_eq!(out.status.code(), Some(2), "{all}");
    assert!(all.contains("server at"), "{all}");
    assert!(all.contains("standalone role at"), "{all}");
    assert!(all.contains("--context"), "{all}");
    assert!(all.contains("mllm context add"), "{all}");
}

// T01: `context add|use|list|remove`. The stored token and the contexts file
// are owner-only (0600) under the config home; the token is never printed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn contexts_are_added_used_listed_and_removed() {
    let (address, api) = api(SERVER_TOKEN, "saved").await;
    let machine = Machine::new();
    let key = machine.root.path().join("server.key");
    private_file(&key, &format!("{SERVER_TOKEN}\n"));
    let address_text = address.to_string();
    let out = machine
        .run(
            &[
                "context",
                "add",
                "lab",
                "--server",
                &address_text,
                "--key-file",
                key.to_str().unwrap(),
            ],
            &[],
        )
        .await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(!text(&out).contains(SERVER_TOKEN), "{}", text(&out));
    let config = machine.home().join(".config/mllm");
    for file in [
        config.join("contexts.yaml"),
        config.join("contexts/lab.key"),
    ] {
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{}", file.display());
    }
    let yaml = std::fs::read_to_string(config.join("contexts.yaml")).unwrap();
    assert!(!yaml.contains(SERVER_TOKEN), "{yaml}");

    // Not current yet, and nothing runs here: a client command is refused.
    let out = machine.run(&["list", "hosts", "--json"], &[]).await;
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    let out = machine.run(&["context", "use", "lab"], &[]).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let out = machine.run(&["list", "hosts", "--json"], &[]).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(reached(&out), "saved");
    assert!(api.calls.load(Ordering::SeqCst) >= 1);

    let out = machine.run(&["context", "list"], &[]).await;
    let table = String::from_utf8_lossy(&out.stdout).to_string();
    let row = table.lines().find(|l| l.contains("lab")).unwrap();
    assert!(row.starts_with('*'), "{table}");
    assert!(row.contains(&address_text), "{table}");
    assert!(!table.contains(SERVER_TOKEN), "{table}");

    let out = machine.run(&["context", "remove", "lab"], &[]).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(!config.join("contexts/lab.key").exists());
    let out = machine.run(&["context", "list", "--json"], &[]).await;
    let listed: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(listed["contexts"], json!([]), "{listed}");
    assert_eq!(listed["current"], Value::Null, "{listed}");
}

// T01: `--context` (or `--config`) > MLLM_CONTEXT > the current context >
// auto-detection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_flag_wins_over_the_environment_which_wins_over_the_current_context() {
    let (local, _) = api(STANDALONE_TOKEN, "local").await;
    let (a, _) = api(SERVER_TOKEN, "a").await;
    let (b, _) = api(SERVER_TOKEN, "b").await;
    let (c, _) = api(SERVER_TOKEN, "c").await;
    let machine = Machine::new();
    machine.standalone().recorded(local);
    let key = machine.root.path().join("server.key");
    private_file(&key, SERVER_TOKEN);
    for (name, address) in [("a", a), ("b", b), ("c", c)] {
        let address = address.to_string();
        let out = machine
            .run(
                &[
                    "context",
                    "add",
                    name,
                    "--server",
                    &address,
                    "--key-file",
                    key.to_str().unwrap(),
                ],
                &[],
            )
            .await;
        assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    }
    let hosts = ["list", "hosts", "--json"];
    assert_eq!(reached(&machine.run(&hosts, &[]).await), "local");
    machine.run(&["context", "use", "a"], &[]).await;
    assert_eq!(reached(&machine.run(&hosts, &[]).await), "a");
    assert_eq!(
        reached(&machine.run(&hosts, &[("MLLM_CONTEXT", "b")]).await),
        "b"
    );
    let flagged = ["list", "hosts", "--json", "--context", "c"];
    assert_eq!(
        reached(&machine.run(&flagged, &[("MLLM_CONTEXT", "b")]).await),
        "c"
    );
    let both = machine
        .run(
            &[
                "list",
                "hosts",
                "--context",
                "c",
                "--config",
                "/nonexistent.yaml",
            ],
            &[],
        )
        .await;
    assert_eq!(both.status.code(), Some(2), "{}", text(&both));
    let missing = machine.run(&hosts, &[("MLLM_CONTEXT", "nope")]).await;
    assert_ne!(missing.status.code(), Some(0), "{}", text(&missing));
    assert!(
        text(&missing).contains("mllm context list"),
        "{}",
        text(&missing)
    );
}

// T01: the admin token is never a command-line value: `context add` takes it
// from a file or MLLM_CONTEXT_KEY only, refuses a flag carrying it, and a
// non-loopback address is refused (management is served on loopback only).
#[test]
fn a_context_key_never_rides_the_command_line() {
    let machine = Machine::new();
    let refused = machine.mllm(
        &[
            "context",
            "add",
            "x",
            "--server",
            "127.0.0.1:7443",
            "--key",
            SERVER_TOKEN,
        ],
        &[],
    );
    assert_eq!(refused.status.code(), Some(2), "{}", text(&refused));
    let missing = machine.mllm(&["context", "add", "x", "--server", "127.0.0.1:7443"], &[]);
    assert_eq!(missing.status.code(), Some(2), "{}", text(&missing));
    assert!(
        text(&missing).contains("MLLM_CONTEXT_KEY"),
        "{}",
        text(&missing)
    );
    let from_env = machine.mllm(
        &["context", "add", "x", "--server", "127.0.0.1:7443"],
        &[("MLLM_CONTEXT_KEY", SERVER_TOKEN)],
    );
    assert_eq!(from_env.status.code(), Some(0), "{}", text(&from_env));
    assert!(!text(&from_env).contains(SERVER_TOKEN));
    let stored =
        std::fs::read_to_string(machine.home().join(".config/mllm/contexts/x.key")).unwrap();
    assert_eq!(stored.trim(), SERVER_TOKEN);
    let remote = machine.mllm(
        &["context", "add", "y", "--server", "192.0.2.10:7443"],
        &[("MLLM_CONTEXT_KEY", SERVER_TOKEN)],
    );
    assert_eq!(remote.status.code(), Some(2), "{}", text(&remote));
    assert!(text(&remote).contains("ssh -N -L"), "{}", text(&remote));
    // A key file others can read is refused when the context is used.
    std::fs::set_permissions(
        machine.home().join(".config/mllm/contexts/x.key"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let loose = machine.mllm(&["list", "hosts", "--context", "x"], &[]);
    assert_eq!(loose.status.code(), Some(2), "{}", text(&loose));
    assert!(text(&loose).contains("0600"), "{}", text(&loose));
}
