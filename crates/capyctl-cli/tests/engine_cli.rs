//! ADR 0018: `capyctl engine` against fake environments and a scripted role
//! socket. CPU tests only; they are not qualification.
mod support;

use capyctl_agent::control_socket::{ControlHandler, ControlRequest, ControlServer};
use capyctl_cli::engine::{execute, execute_with, resolve_target};
use capyctl_cli::grammar::{Command, DeepParkChoice, DriftChoice};
use capyctl_config::registration::{engines_beside, EnginesFile};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

fn private_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn script(path: &Path, body: &str) {
    capyctl_config::test_support::write_executable(path, format!("#!/bin/sh\n{body}\n"), 0o755)
        .unwrap();
}

/// A vLLM venv: `vllm --version` prints `reported`; the interpreter answers
/// the capability probe with `deep_park_missing` labels.
fn vllm_env(root: &Path, version: &str, reported: &str, deep_park_missing: &[&str]) -> PathBuf {
    let site = root.join("lib/python3.12/site-packages");
    std::fs::create_dir_all(site.join("vllm")).unwrap();
    std::fs::create_dir_all(site.join(format!("vllm-{version}.dist-info"))).unwrap();
    std::fs::write(
        site.join(format!("vllm-{version}.dist-info/METADATA")),
        format!("Name: vllm\nVersion: {version}\n"),
    )
    .unwrap();
    std::fs::write(root.join("pyvenv.cfg"), "home = /usr/bin\n").unwrap();
    std::fs::create_dir_all(root.join("bin")).unwrap();
    script(&root.join("bin/vllm"), &format!("echo {reported}"));
    let report = json!({"schema": "capyctl/engine-capabilities/v1", "engine": "vllm",
        "capabilities": {"core": [], "deep_park": deep_park_missing, "metrics": []}});
    script(&root.join("bin/python3"), &format!("echo '{report}'"));
    root.to_path_buf()
}

/// A host document whose state directory is private and short.
fn host_doc(dir: &Path) -> PathBuf {
    let state = dir.join("s");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.join("host.yaml");
    std::fs::write(
        &path,
        capyctl_config::remote_roles::HostConfig::template(&state),
    )
    .unwrap();
    path
}

fn engines_of(document: &Path) -> EnginesFile {
    EnginesFile::load(&engines_beside(document)).unwrap()
}

struct Role(Mutex<Vec<ControlRequest>>, Value);
#[async_trait::async_trait]
impl ControlHandler for Role {
    async fn handle(&self, request: ControlRequest) -> Value {
        self.0.lock().unwrap().push(request);
        self.1.clone()
    }
}

async fn role(document: &Path, reply: Value) -> (Arc<Role>, tokio::sync::watch::Sender<bool>) {
    let target = resolve_target(Some(document), Path::new("/nonexistent"), &|k| {
        (k == "HOME").then(|| "/home/u".into())
    })
    .unwrap();
    let server = ControlServer::bind(&target.socket).unwrap();
    let handler = Arc::new(Role(Mutex::new(vec![]), reply));
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    tokio::spawn(server.serve(handler.clone(), unsafe { libc::geteuid() }, shutdown));
    (handler, stop)
}

/// Waits until the stopped role has removed its socket, which it does as it
/// stops; a slow runner gets there later.
async fn until_role_stopped(document: &Path) {
    let target = resolve_target(Some(document), Path::new("/nonexistent"), &|k| {
        (k == "HOME").then(|| "/home/u".into())
    })
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while target.socket.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the stopped role removed its socket");
}

fn add(path: &Path) -> Command {
    Command::EngineAdd {
        path: Some(path.into()),
        name: None,
        deep_park: None,
        drift: DriftChoice::Warn,
        args: vec![],
    }
}

// T07 (ADR 0018 §1–§3): add writes the profile into engines.yaml beside the
// host document, which is never touched, and asks the role to publish it.
#[tokio::test]
async fn add_writes_the_profile_and_publishes() {
    let dir = private_dir();
    let env = vllm_env(&dir.path().join("v"), "0.29.0", "0.29.0", &[]);
    let document = host_doc(dir.path());
    let host_before = std::fs::read(&document).unwrap();
    let (role, _stop) = role(&document, json!({"ok": true, "published": "published"})).await;
    let out = execute(&add(&env), Some(&document), dir.path())
        .await
        .unwrap();
    assert_eq!(out["published"], "published");
    assert_eq!(out["custom"], false);
    let engines = engines_of(&document);
    assert_eq!(engines.revision, 1);
    let profile = &engines.profiles["vllm"];
    assert_eq!(
        profile["executable"],
        env.join("bin/vllm").to_string_lossy().as_ref()
    );
    assert_eq!(profile["build_fingerprint"], "0.29.0");
    assert_eq!(profile["security"]["deep_park"], "enabled");
    assert_eq!(
        std::fs::read(&document).unwrap(),
        host_before,
        "host.yaml is never rewritten"
    );
    assert_eq!(*role.0.lock().unwrap(), vec![ControlRequest::Add]);
}

// T03 (owner decision 2026-09-25): a name registered already, or declared in
// the host document, is refused and nothing is written.
#[tokio::test]
async fn add_refuses_an_existing_name() {
    let dir = private_dir();
    let env = vllm_env(&dir.path().join("v"), "0.29.0", "0.29.0", &[]);
    let document = host_doc(dir.path());
    let (_role, _stop) = role(&document, json!({"ok": true, "published": "published"})).await;
    execute(&add(&env), Some(&document), dir.path())
        .await
        .unwrap();
    let before = std::fs::read(engines_beside(&document)).unwrap();
    let error = execute(&add(&env), Some(&document), dir.path())
        .await
        .unwrap_err();
    assert_eq!(error.code, "profile_exists");
    assert_eq!(std::fs::read(engines_beside(&document)).unwrap(), before);
    let mut host: Value =
        serde_json::from_str(&std::fs::read_to_string(&document).unwrap()).unwrap();
    host["runtime_profiles"]["sg"] = engines_of(&document).profiles["vllm"].clone();
    std::fs::write(&document, host.to_string()).unwrap();
    let named = Command::EngineAdd {
        path: Some(env.clone()),
        name: Some("sg".into()),
        deep_park: None,
        drift: DriftChoice::Warn,
        args: vec![],
    };
    assert_eq!(
        execute(&named, Some(&document), dir.path())
            .await
            .unwrap_err()
            .code,
        "profile_exists"
    );
}

// ADR 0018 §3: a refused publication is reported; the profile stays written.
#[tokio::test]
async fn add_reports_a_rejected_publication() {
    let dir = private_dir();
    let env = vllm_env(&dir.path().join("v"), "0.29.0", "0.29.0", &[]);
    let document = host_doc(dir.path());
    let (_role, _stop) = role(
        &document,
        json!({"ok": false, "code": "publish_rejected", "message": "no"}),
    )
    .await;
    let error = execute(&add(&env), Some(&document), dir.path())
        .await
        .unwrap_err();
    assert_eq!(error.code, "publish_rejected");
    assert!(engines_of(&document).profiles.contains_key("vllm"));
}

// ADR 0018 §3 (owner decision 2026-09-25): without a running role
// engines.yaml is written and the command succeeds, saying the profile takes
// effect when the role starts.
#[tokio::test]
async fn add_without_a_running_role_saves_and_succeeds() {
    let dir = private_dir();
    let env = vllm_env(&dir.path().join("v"), "0.29.0", "0.29.0", &[]);
    let document = host_doc(dir.path());
    let out = execute(&add(&env), Some(&document), dir.path())
        .await
        .unwrap();
    assert_eq!(out["published"], "role_not_running");
    let notice = out["notice"].as_str().unwrap();
    assert!(
        notice.contains("revision 1") && notice.contains("capyctl start host"),
        "{notice}"
    );
    // Release live check 0.1.1: a role running with another state directory
    // is not found here; the notice names the socket it looked for and how to
    // point at the running role.
    assert!(
        notice.contains(&format!("{}", dir.path().join("s/control.sock").display()))
            && notice.contains("--state-dir")
            && notice.contains("restart it"),
        "{notice}"
    );
    assert!(engines_of(&document).profiles.contains_key("vllm"));
}

// T02 T07 (review decision 2026-09-25): `engine add` before any role has
// ever started is the first run. On a fresh HOME with no state directory and
// no role document it creates the state root owner-only, writes engines.yaml
// where `start standalone` reads it, and succeeds with a notice.
#[tokio::test]
async fn add_on_a_fresh_home_is_the_first_run() {
    use std::os::unix::fs::MetadataExt;
    let dir = private_dir();
    let env = vllm_env(&dir.path().join("v"), "0.29.0", "0.29.0", &[]);
    let home = dir.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let state = home.join(".local/state/capyctl");
    assert!(!state.exists());
    let home_text = home.to_string_lossy().into_owned();
    let fresh = move |key: &str| (key == "HOME").then(|| home_text.clone());
    let out = capyctl_cli::engine::execute_in(&add(&env), None, &state, &fresh)
        .await
        .unwrap();
    assert_eq!(out["published"], "role_not_running", "{out}");
    let meta = std::fs::metadata(&state).unwrap();
    assert!(meta.is_dir());
    assert_eq!(meta.mode() & 0o777, 0o700, "the state root is owner-only");
    // The probe's scratch directory is gone; nothing else is left behind.
    assert_eq!(std::fs::read_dir(&state).unwrap().count(), 0);
    let engines = EnginesFile::load(&home.join(".config/capyctl/engines.yaml")).unwrap();
    assert_eq!(engines.revision, 1);
    assert!(engines.profiles.contains_key("vllm"));
    // The same command lists it, as not yet seen by a role.
    let listed = capyctl_cli::engine::execute_in(&Command::EngineList, None, &state, &fresh)
        .await
        .unwrap();
    assert_eq!(listed["agent"], "unreachable");
    assert_eq!(listed["engines"][0]["profile"], "vllm");
}

// T34 (ADR 0017 fallback): a peer without live_profile_update means restart.
#[tokio::test]
async fn add_restart_required_is_success_with_notice() {
    let dir = private_dir();
    let env = vllm_env(&dir.path().join("v"), "0.29.0", "0.29.0", &[]);
    let document = host_doc(dir.path());
    let (_role, _stop) = role(
        &document,
        json!({"ok": true, "published": "restart_required"}),
    )
    .await;
    let out = execute(&add(&env), Some(&document), dir.path())
        .await
        .unwrap();
    assert_eq!(out["published"], "restart_required");
}

// T37: a version check that disagrees with the metadata writes nothing.
#[tokio::test]
async fn add_version_mismatch_writes_nothing() {
    let dir = private_dir();
    let env = vllm_env(&dir.path().join("v"), "0.29.0", "0.28.0", &[]);
    let document = host_doc(dir.path());
    let error = execute(&add(&env), Some(&document), dir.path())
        .await
        .unwrap_err();
    assert_eq!(error.code, "engine_version_failed");
    assert!(!engines_beside(&document).exists());
}

// T21 (owner decision 2026-09-25): a probe that reports deep park missing
// writes it disabled, unless the operator asked for enabled.
#[tokio::test]
async fn add_disables_deep_park_when_the_probe_reports_it_missing() {
    let dir = private_dir();
    let env = vllm_env(&dir.path().join("v"), "0.29.0", "0.29.0", &["sleep_mode"]);
    let document = host_doc(dir.path());
    let (_role, _stop) = role(&document, json!({"ok": true, "published": "published"})).await;
    let out = execute(&add(&env), Some(&document), dir.path())
        .await
        .unwrap();
    assert_eq!(out["deep_park_probe"], "capability_missing");
    assert_eq!(
        engines_of(&document).profiles["vllm"]["security"]["deep_park"],
        "disabled"
    );
    let asked = Command::EngineAdd {
        path: Some(env.clone()),
        name: Some("vllm-deep".into()),
        deep_park: Some(DeepParkChoice::Enabled),
        drift: DriftChoice::Warn,
        args: vec![],
    };
    execute(&asked, Some(&document), dir.path()).await.unwrap();
    assert_eq!(
        engines_of(&document).profiles["vllm-deep"]["security"]["deep_park"],
        "enabled"
    );
}

// T01: add without a path needs a terminal (tests run without one).
#[tokio::test]
async fn add_without_a_path_needs_a_terminal() {
    // A developer's `cargo test` may inherit a terminal; take it away so the
    // command never waits on a prompt.
    let null = std::fs::File::open("/dev/null").unwrap();
    // SAFETY: replaces this test process's stdin with /dev/null; no test reads it.
    assert!(unsafe { libc::dup2(std::os::fd::AsRawFd::as_raw_fd(&null), 0) } == 0);
    let dir = private_dir();
    let document = host_doc(dir.path());
    let command = Command::EngineAdd {
        path: None,
        name: None,
        deep_park: None,
        drift: DriftChoice::Warn,
        args: vec![],
    };
    assert_eq!(
        execute(&command, Some(&document), dir.path())
            .await
            .unwrap_err()
            .code,
        "not_interactive"
    );
}

// T37: detect lists the fake environment and runs nothing in it.
#[tokio::test]
async fn detect_lists_candidates_and_runs_nothing() {
    let dir = private_dir();
    let env = vllm_env(&dir.path().join("envs/v"), "0.29.0", "0.29.0", &[]);
    let marker = dir.path().join("ran");
    script(
        &env.join("bin/vllm"),
        &format!("touch {}", marker.display()),
    );
    let out = execute(
        &Command::EngineDetect {
            paths: vec![dir.path().join("envs")],
        },
        None,
        dir.path(),
    )
    .await
    .unwrap();
    let found = out["candidates"].as_array().unwrap();
    assert!(
        found
            .iter()
            .any(|c| c["env"] == env.to_string_lossy().as_ref() && c["engine"] == "vllm"),
        "{out}"
    );
    assert!(!marker.exists());
}

// Owner decision 2026-09-25: which role document and which engines file.
#[test]
fn target_resolution_follows_the_documented_order() {
    let dir = private_dir();
    let explicit = host_doc(dir.path());
    let config_home = dir.path().join("cfg");
    std::fs::create_dir_all(config_home.join("capyctl")).unwrap();
    std::fs::copy(&explicit, config_home.join("capyctl/host.yaml")).unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir_all(state.join("config")).unwrap();
    std::fs::write(
        state.join("config/standalone.yaml"),
        "schema_version: 1\nkind: standalone\nname: local\n",
    )
    .unwrap();
    let cfg = config_home.to_string_lossy().into_owned();
    let env = |key: &str| (key == "XDG_CONFIG_HOME").then(|| cfg.clone());
    // --config dir/x.yaml → dir/engines.yaml.
    let t = resolve_target(Some(&explicit), &state, &env).unwrap();
    assert_eq!(
        (t.role_document.clone(), t.engines.clone()),
        (explicit.clone(), dir.path().join("engines.yaml"))
    );
    // Both implicit documents: ambiguous.
    assert_eq!(
        resolve_target(None, &state, &env).unwrap_err().code,
        "invalid_config"
    );
    // Implicit standalone: its document in the state dir, engines in the config home.
    std::fs::remove_file(config_home.join("capyctl/host.yaml")).unwrap();
    let t = resolve_target(None, &state, &env).unwrap();
    assert_eq!(t.kind, capyctl_cli::engine::RoleKind::Standalone);
    assert_eq!(t.engines, config_home.join("capyctl/engines.yaml"));
    // Implicit host: the same engines file.
    std::fs::remove_file(state.join("config/standalone.yaml")).unwrap();
    std::fs::copy(&explicit, config_home.join("capyctl/host.yaml")).unwrap();
    let t = resolve_target(None, &state, &env).unwrap();
    assert_eq!(
        (t.kind, t.engines),
        (
            capyctl_cli::engine::RoleKind::Host,
            config_home.join("capyctl/engines.yaml")
        )
    );
    // $CAPYCTL_CONFIG counts as naming the document.
    let path = explicit.to_string_lossy().into_owned();
    let named = |key: &str| match key {
        "CAPYCTL_CONFIG" => Some(path.clone()),
        "XDG_CONFIG_HOME" => Some(cfg.clone()),
        _ => None,
    };
    assert_eq!(
        resolve_target(None, &state, &named).unwrap().engines,
        dir.path().join("engines.yaml")
    );
}

// T16 T32: removal in use is refused with the list, nothing written.
#[tokio::test]
async fn remove_in_use_is_refused_with_the_list() {
    let dir = private_dir();
    let env = vllm_env(&dir.path().join("v"), "0.29.0", "0.29.0", &[]);
    let document = host_doc(dir.path());
    {
        let (_role, stop) = role(&document, json!({"ok": true, "published": "published"})).await;
        execute(&add(&env), Some(&document), dir.path())
            .await
            .unwrap();
        stop.send(true).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let (_role, _stop) = role(
        &document,
        json!({"ok": false, "code": "profile_in_use", "deployments": ["q14"], "message": "in use"}),
    )
    .await;
    let error = execute(
        &Command::EngineRemove {
            name: "vllm".into(),
            drain: false,
        },
        Some(&document),
        dir.path(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "profile_in_use");
    assert!(error.message.contains("q14"), "{}", error.message);
}

// Owner decision 2026-09-25: without a role nothing is removed; a profile the
// operator declared in host.yaml is never removed by capyctl.
#[tokio::test]
async fn remove_without_a_role_writes_nothing() {
    let dir = private_dir();
    let env = vllm_env(&dir.path().join("v"), "0.29.0", "0.29.0", &[]);
    let document = host_doc(dir.path());
    let error = execute(
        &Command::EngineRemove {
            name: "vllm".into(),
            drain: true,
        },
        Some(&document),
        dir.path(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "invalid_config", "no such profile");
    let _ = execute(&add(&env), Some(&document), dir.path()).await;
    let before = std::fs::read(engines_beside(&document)).unwrap();
    let error = execute(
        &Command::EngineRemove {
            name: "vllm".into(),
            drain: true,
        },
        Some(&document),
        dir.path(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "agent_unreachable");
    assert_eq!(std::fs::read(engines_beside(&document)).unwrap(), before);
    let mut host: Value =
        serde_json::from_str(&std::fs::read_to_string(&document).unwrap()).unwrap();
    host["runtime_profiles"]["theirs"] = engines_of(&document).profiles["vllm"].clone();
    std::fs::write(&document, host.to_string()).unwrap();
    let error = execute(
        &Command::EngineRemove {
            name: "theirs".into(),
            drain: false,
        },
        Some(&document),
        dir.path(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "invalid_config");
    assert!(
        error.message.contains("edit that file"),
        "{}",
        error.message
    );
}

// ADR 0018 §1: list shows registered and declared profiles with what the
// role accepted.
#[tokio::test]
async fn list_merges_the_files_and_the_role() {
    let dir = private_dir();
    let env = vllm_env(&dir.path().join("v"), "0.29.0", "0.29.0", &[]);
    let document = host_doc(dir.path());
    let (_r, stop) = role(&document, json!({"ok": true, "published": "published"})).await;
    execute(&add(&env), Some(&document), dir.path())
        .await
        .unwrap();
    stop.send(true).unwrap();
    until_role_stopped(&document).await;
    let offline = execute(&Command::EngineList, Some(&document), dir.path())
        .await
        .unwrap();
    assert_eq!(offline["agent"], "unreachable");
    assert_eq!(offline["engines"][0]["published"], "unknown");
    assert_eq!(offline["engines"][0]["source"], "engines.yaml");
    let (_r, _stop) = role(
        &document,
        json!({"ok": true, "connected": true, "live_profile_update": true,
        "accepted": {}, "users": {}}),
    )
    .await;
    let listed = execute(&Command::EngineList, Some(&document), dir.path())
        .await
        .unwrap();
    assert_eq!(listed["engines"][0]["published"], "not published");
}

// T16 (ADR 0018 §5): a standalone role's environment profiles (`local`,
// `local-vllm`, `local-sglang`) are in neither engines.yaml nor the role
// document, but the role publishes them, so `engine list` shows them as
// published (found live 2026-09-25: the list came back empty).
#[tokio::test]
async fn list_shows_the_roles_environment_profiles() {
    let dir = private_dir();
    let document = host_doc(dir.path());
    let accepted = |engine: &str, exe: &str, version: &str| {
        json!({"engine": engine, "executable": exe, "build_fingerprint": version,
        "installation": {"version": version, "digest": "sha256:00", "state": "recorded"},
        "deep_park": "enabled", "deep_park_probe": "unknown"})
    };
    let (_r, _stop) = role(
        &document,
        json!({"ok": true, "connected": true, "live_profile_update": true,
        "accepted": {"local-vllm": accepted("vllm", "/v/bin/vllm", "0.29.0"),
                     "local-sglang": accepted("sglang", "/s/bin/python3", "0.5.20")},
        "users": {"local-vllm": ["m"]}}),
    )
    .await;
    let listed = execute(&Command::EngineList, Some(&document), dir.path())
        .await
        .unwrap();
    let rows = listed["engines"].as_array().unwrap();
    let row = |name: &str| rows.iter().find(|r| r["profile"] == name).cloned();
    let v = row("local-vllm").unwrap_or_else(|| panic!("{listed}"));
    assert_eq!(v["source"], "environment");
    assert_eq!(v["published"], "published");
    assert_eq!(v["engine"], "vllm");
    assert_eq!(v["version"], "0.29.0");
    assert_eq!(v["custom"], false);
    assert_eq!(v["executable"], "/v/bin/vllm");
    assert_eq!(v["deep_park"], "enabled");
    assert_eq!(v["deployments"], json!(["m"]));
    let s = row("local-sglang").unwrap_or_else(|| panic!("{listed}"));
    assert_eq!(s["source"], "environment");
    assert_eq!(s["engine"], "sglang");
    assert_eq!(s["published"], "published");
}

// T01: `list engines` is a server command.
#[test]
fn list_engines_goes_to_the_server() {
    assert!(capyctl_cli::remote_roles::supports(&Command::List {
        resource: capyctl_cli::grammar::ListResource::Engines
    }));
}

// T37 (ADR 0018 §4; review decision I4): a remove the role took but never
// answered (it closed the connection, or the bound passed) is reported as an
// unknown outcome that `capyctl engine list` settles, never as "nothing was
// removed"; engines.yaml is not touched.
#[tokio::test]
async fn remove_without_an_answer_reports_an_unknown_outcome() {
    use std::io::BufRead;
    let dir = private_dir();
    let env = vllm_env(&dir.path().join("v"), "0.29.0", "0.29.0", &[]);
    let document = host_doc(dir.path());
    let _ = execute(&add(&env), Some(&document), dir.path()).await;
    let before = std::fs::read(engines_beside(&document)).unwrap();
    let socket = resolve_target(Some(&document), Path::new("/nonexistent"), &|k| {
        (k == "HOME").then(|| "/home/u".into())
    })
    .unwrap()
    .socket;
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let role = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        std::io::BufReader::new(&stream)
            .read_line(&mut line)
            .unwrap();
        drop(stream);
    });
    let error = execute(
        &Command::EngineRemove {
            name: "vllm".into(),
            drain: true,
        },
        Some(&document),
        dir.path(),
    )
    .await
    .unwrap_err();
    role.join().unwrap();
    assert_eq!(error.code, "agent_unreachable");
    assert!(
        error.message.contains("outcome is unknown")
            && error.message.contains("capyctl engine list")
            && !error.message.contains("nothing was removed"),
        "{}",
        error.message
    );
    assert_eq!(std::fs::read(engines_beside(&document)).unwrap(), before);
}

/// A role that answers `remove` and `add` each its own way.
struct ByOp {
    seen: Mutex<Vec<ControlRequest>>,
    remove: Value,
    add: Value,
}
#[async_trait::async_trait]
impl ControlHandler for ByOp {
    async fn handle(&self, request: ControlRequest) -> Value {
        self.seen.lock().unwrap().push(request.clone());
        match request {
            ControlRequest::Remove { .. } => self.remove.clone(),
            ControlRequest::Add => self.add.clone(),
            ControlRequest::List => json!({"ok": false, "code": "internal"}),
        }
    }
}

async fn role_by_op(
    document: &Path,
    remove: Value,
    add: Value,
) -> (Arc<ByOp>, tokio::sync::watch::Sender<bool>) {
    let target = resolve_target(Some(document), Path::new("/nonexistent"), &|k| {
        (k == "HOME").then(|| "/home/u".into())
    })
    .unwrap();
    let server = ControlServer::bind(&target.socket).unwrap();
    let handler = Arc::new(ByOp {
        seen: Mutex::new(vec![]),
        remove,
        add,
    });
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    tokio::spawn(server.serve(handler.clone(), unsafe { libc::geteuid() }, shutdown));
    (handler, stop)
}

fn remove_vllm() -> Command {
    Command::EngineRemove {
        name: "vllm".into(),
        drain: false,
    }
}

// T16 (ADR 0018 §4; review decision C1): the CLI is the only writer of
// engines.yaml. It asks the role to retire the profile, and only after the
// confirmation writes the file (keeping its mode) and asks for the reload.
#[tokio::test]
async fn remove_retires_then_writes_and_reloads() {
    let dir = private_dir();
    let env = vllm_env(&dir.path().join("v"), "0.29.0", "0.29.0", &[]);
    let document = host_doc(dir.path());
    let _ = execute(&add(&env), Some(&document), dir.path()).await;
    let (role, stop) = role_by_op(
        &document,
        json!({"ok": true, "retired": true}),
        json!({"ok": true, "published": "published"}),
    )
    .await;
    let out = execute(&remove_vllm(), Some(&document), dir.path())
        .await
        .unwrap();
    assert_eq!(out["removed"], "vllm", "{out}");
    assert_eq!(out["published"], "published", "{out}");
    assert_eq!(out["revision"], 2, "{out}");
    let engines = engines_of(&document);
    assert!(!engines.profiles.contains_key("vllm"));
    assert_eq!(
        std::fs::metadata(engines_beside(&document))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        *role.seen.lock().unwrap(),
        vec![
            ControlRequest::Remove {
                profile: "vllm".into(),
                drain: false
            },
            ControlRequest::Add
        ]
    );
    stop.send(true).unwrap();
}

// T16 T32 (review decisions C1, I1): a remove whose confirmation came but
// whose publication did not (the file is already written) is finished by
// running it again: the role still publishes the profile, the retry resumes
// the retirement, and the reload publishes the removal. A name neither
// registered nor published is refused.
#[tokio::test]
async fn a_rerun_remove_finishes_a_removal_the_file_already_shows() {
    let dir = private_dir();
    let document = host_doc(dir.path());
    let (role, stop) = role_by_op(
        &document,
        json!({"ok": true, "retired": true}),
        json!({"ok": true, "published": "published"}),
    )
    .await;
    let out = execute(&remove_vllm(), Some(&document), dir.path())
        .await
        .unwrap();
    assert_eq!(out["removed"], "vllm", "{out}");
    assert_eq!(out["published"], "published", "{out}");
    assert_eq!(role.seen.lock().unwrap().len(), 2);
    stop.send(true).unwrap();
    until_role_stopped(&document).await;
    let (_role, stop) = role_by_op(
        &document,
        json!({"ok": true, "retired": false}),
        json!({"ok": true, "published": "unchanged"}),
    )
    .await;
    let error = execute(&remove_vllm(), Some(&document), dir.path())
        .await
        .unwrap_err();
    assert_eq!(error.code, "invalid_config", "{}", error.message);
    stop.send(true).unwrap();
}

fn capyctl(home: &Path, args: &[&str]) -> String {
    let out = support::capyctl()
        .env("HOME", home)
        .env("PATH", "/nonexistent")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("PIPX_HOME")
        .env_remove("CAPYCTL_CONFIG")
        .env("CAPYCTL_STATE_DIR", home.join("state"))
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

// T37, owner decision 2026-09-25: `engine detect` and `engine list` print an
// aligned table by default; `--format json` prints the JSON result unchanged
// (the same bytes as `--output json`).
#[test]
fn engine_views_print_tables_and_json_on_request() {
    let dir = private_dir();
    let env = vllm_env(&dir.path().join("envs/v"), "0.29.0", "0.29.0", &[]);
    let envs = dir.path().join("envs");
    let detect = ["engine", "detect", "--path", envs.to_str().unwrap()];
    let table = capyctl(dir.path(), &detect);
    let json = capyctl(dir.path(), &[&detect[..], &["--format", "json"]].concat());
    assert_eq!(
        json,
        capyctl(dir.path(), &[&detect[..], &["--output", "json"]].concat())
    );
    let value: Value = serde_json::from_str(&json).unwrap();
    let rows = value["candidates"].as_array().unwrap().len();
    let lines: Vec<&str> = table.lines().collect();
    assert_eq!(lines.len(), rows + 1, "{table}");
    assert!(
        lines[0].starts_with("ENGINE   VERSION   CUSTOM   ENVIRONMENT"),
        "{table}"
    );
    let env = env.to_string_lossy();
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("vllm     0.29.0    ") && l.contains(env.as_ref())),
        "{table}"
    );

    let document = host_doc(dir.path());
    let list = ["engine", "list", "--config", document.to_str().unwrap()];
    let table = capyctl(dir.path(), &list);
    assert!(
        table.starts_with(
            "PROFILE   SOURCE   ENGINE   VERSION   CUSTOM   DEEP PARK   PUBLISHED   DEPLOYMENTS\n"
        ),
        "{table}"
    );
    let json = capyctl(dir.path(), &[&list[..], &["--format", "json"]].concat());
    let value: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["agent"], "unreachable", "{json}");
    assert_eq!(
        table.lines().count(),
        value["engines"].as_array().unwrap().len() + 1,
        "{table}"
    );
}

/// A TensorFold venv: `tensorfold --version` prints `tensorfold <version>`,
/// the interpreter answers the probe, and `bin` holds the build tools.
fn tensorfold_env(root: &Path, version: &str, tools: &[&str]) -> PathBuf {
    let site = root.join("lib/python3.12/site-packages");
    std::fs::create_dir_all(site.join("tensorfold")).unwrap();
    std::fs::create_dir_all(site.join(format!("tensorfold-{version}.dist-info"))).unwrap();
    std::fs::write(
        site.join(format!("tensorfold-{version}.dist-info/METADATA")),
        format!("Name: tensorfold\nVersion: {version}\n"),
    )
    .unwrap();
    std::fs::write(root.join("pyvenv.cfg"), "home = /usr/bin\n").unwrap();
    std::fs::create_dir_all(root.join("bin")).unwrap();
    script(
        &root.join("bin/tensorfold"),
        &format!("echo tensorfold {version}"),
    );
    let report = json!({"schema": "capyctl/engine-capabilities/v1", "engine": "tensorfold",
        "capabilities": {"core": [], "deep_park": ["unsupported"], "metrics": []}});
    script(&root.join("bin/python3"), &format!("echo '{report}'"));
    for tool in tools {
        script(&root.join("bin").join(tool), "exit 0");
    }
    root.to_path_buf()
}

// T41 T07 T21: TensorFold registers as `tensorfold`, deep park disabled.
#[tokio::test]
async fn add_registers_tensorfold_with_deep_park_disabled() {
    let dir = private_dir();
    let env = tensorfold_env(&dir.path().join("tf"), "0.6.0", &["ninja", "nvcc", "c++"]);
    let document = host_doc(dir.path());
    let (_role, _stop) = role(&document, json!({"ok": true, "published": "published"})).await;
    let out = execute(&add(&env), Some(&document), dir.path())
        .await
        .unwrap();
    assert_eq!(out["profile"], "tensorfold");
    assert_eq!(out["engine"], "tensorfold");
    assert_eq!(out["custom"], false);
    let profile = &engines_of(&document).profiles["tensorfold"];
    assert_eq!(
        profile["executable"],
        env.join("bin/tensorfold").to_string_lossy().as_ref()
    );
    assert_eq!(profile["build_fingerprint"], "0.6.0");
    assert_eq!(profile["security"]["deep_park"], "disabled");
    let asked = Command::EngineAdd {
        path: Some(env.clone()),
        name: Some("tf-deep".into()),
        deep_park: Some(DeepParkChoice::Enabled),
        drift: DriftChoice::Warn,
        args: vec![],
    };
    let error = execute(&asked, Some(&document), dir.path())
        .await
        .unwrap_err();
    assert_eq!(error.code, "capability_missing");
    assert!(!engines_of(&document).profiles.contains_key("tf-deep"));
}

// T41 T07: 0.6.1 is verified beside 0.6.0; an unknown 0.6.2 is custom.
#[tokio::test]
async fn tensorfold_061_is_verified_and_062_is_custom() {
    for (version, custom) in [("0.6.1", false), ("0.6.2", true)] {
        let dir = private_dir();
        let env = tensorfold_env(&dir.path().join("tf"), version, &["ninja", "nvcc", "c++"]);
        let document = host_doc(dir.path());
        let (_role, _stop) = role(&document, json!({"ok": true, "published": "published"})).await;
        let out = execute(&add(&env), Some(&document), dir.path())
            .await
            .unwrap();
        assert_eq!(out["version"], version, "{out}");
        assert_eq!(out["custom"], custom, "{out}");
    }
}

// T41 T03: a missing toolchain is refused before anything runs or is written.
// The search names no system directory and no CUDA toolkit, so the result does
// not depend on what this machine has installed.
#[tokio::test]
async fn add_refuses_tensorfold_without_its_toolchain() {
    let dir = private_dir();
    let env = tensorfold_env(&dir.path().join("tf"), "0.6.0", &["ninja", "c++"]);
    let document = host_doc(dir.path());
    let search = capyctl_config::toolchain::ToolchainSearch {
        system: String::new(),
        default_cuda_home: dir.path().join("no-cuda"),
    };
    let process_env = |key: &str| {
        (key != "CUDA_HOME")
            .then(|| std::env::var(key).ok())
            .flatten()
            .filter(|v| !v.is_empty())
    };
    let error = execute_with(
        &add(&env),
        Some(&document),
        dir.path(),
        &process_env,
        &search,
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "toolchain_missing", "{error:?}");
    assert!(error.message.contains("nvcc"), "{}", error.message);
    assert!(error
        .message
        .contains(&env.join("bin").display().to_string()));
    assert!(!engines_beside(&document).exists());
}
