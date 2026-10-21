//! ADR 0031: `capyctl engine add --custom` against fake programs and a
//! scripted role socket. CPU tests only; they are not qualification.
mod support;

use capyctl_agent::control_socket::{ControlHandler, ControlRequest, ControlServer};
use capyctl_cli::engine::{execute, resolve_target};
use capyctl_cli::grammar::{parse, Command, DeepParkChoice, DriftChoice};
use capyctl_cli::output::{ExitCode, StructuredError};
use capyctl_config::custom::CustomManifest;
use capyctl_config::registration::{custom_engines_beside, engines_beside, EnginesFile};
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

/// A virtual environment holding a console script `bin/serve` that touches
/// `marker` if it ever runs, a dependency in `site-packages`, and a
/// `bin/python3` linked to an interpreter outside the tree.
struct Venv {
    root: PathBuf,
    executable: PathBuf,
    interpreter: PathBuf,
    dependency: PathBuf,
}

fn venv(dir: &Path, marker: &Path) -> Venv {
    let root = dir.join("serve-venv");
    let bin = root.join("bin");
    let site = root.join("lib/python3.12/site-packages/serve_dep");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&site).unwrap();
    std::fs::write(root.join("pyvenv.cfg"), "home = /usr/bin\n").unwrap();
    let dependency = site.join("__init__.py");
    std::fs::write(&dependency, "VERSION = 1\n").unwrap();
    let outside = dir.join("python-install");
    std::fs::create_dir_all(&outside).unwrap();
    let interpreter = outside.join("python3.12");
    script(&interpreter, &format!("touch {}", marker.display()));
    std::os::unix::fs::symlink(&interpreter, bin.join("python3")).unwrap();
    let executable = bin.join("serve");
    script(&executable, &format!("touch {}", marker.display()));
    Venv {
        root,
        executable,
        interpreter,
        dependency,
    }
}

const MANIFEST: &str = r#"kind: custom_engine
schema_version: 1
api: decisions
version: "2.2.0"
args: [--mode, eager, --host, "{host}", --port, "{port}", --model, "{model_dir}"]
health: /health
"#;

fn manifest_file(dir: &Path, text: &str) -> PathBuf {
    let path = dir.join(format!("manifest-{}.yaml", ulid::Ulid::new()));
    std::fs::write(&path, text).unwrap();
    path
}

fn add_custom(executable: &Path, name: &str, manifest: &Path) -> Command {
    Command::EngineAddCustom {
        executable: executable.into(),
        name: name.into(),
        manifest: manifest.into(),
        deep_park: None,
        drift: DriftChoice::Warn,
        env: vec![],
        approved_env: vec![],
        yes: true,
    }
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

/// A vLLM venv whose `vllm --version` prints the version; the interpreter
/// answers the capability probe.
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

fn add_vllm(env: &Path, name: Option<&str>) -> Command {
    Command::EngineAdd {
        path: Some(env.into()),
        name: name.map(Into::into),
        deep_park: None,
        drift: DriftChoice::Warn,
        args: vec![],
        approved_options: vec![],
        approved_paths: vec![],
        env: vec![],
        approved_env: vec![],
        allow_remote_code: false,
    }
}

// T44 T07 (ADR 0031 §1, §3, §8, amendment A1): `engine add --custom --yes`
// executes nothing (neither the program nor its interpreter), writes the
// manifest and its digest into custom-engines.yaml, never touches
// engines.yaml or the host document, reports the pinned root, interpreter
// and installation digest, and asks the role to publish.
#[tokio::test]
async fn add_custom_executes_nothing_and_writes_custom_engines_yaml() {
    let dir = private_dir();
    let marker = dir.path().join("ran");
    let venv = venv(dir.path(), &marker);
    let document = host_doc(dir.path());
    let host_before = std::fs::read(&document).unwrap();
    let manifest = manifest_file(dir.path(), MANIFEST);
    let (role, _stop) = role(&document, json!({"ok": true, "published": "published"})).await;
    let out = execute(
        &add_custom(&venv.executable, "basal-serve", &manifest),
        Some(&document),
        dir.path(),
    )
    .await
    .unwrap();
    assert!(!marker.exists(), "engine add --custom executes nothing");
    assert_eq!(out["published"], "published");
    assert_eq!(out["engine"], "custom");
    assert_eq!(out["custom"], true);
    assert_eq!(out["version"], "2.2.0");
    assert_eq!(out["api"], "decisions");
    assert_eq!(out["deep_park"], "disabled");
    assert_eq!(out["idle_signal"], "none");
    assert_eq!(out["engine_key"], "none");
    assert_eq!(
        out["executable"],
        venv.executable.to_string_lossy().as_ref()
    );
    assert_eq!(
        out["root"],
        format!("{} (virtual environment)", venv.root.display())
    );
    assert_eq!(
        out["interpreter"],
        venv.interpreter
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .as_ref()
    );
    assert!(
        out["installation"]
            .as_str()
            .is_some_and(|i| i.starts_with("sha256:")),
        "{out}"
    );
    assert!(out.get("warnings").is_none(), "{out}");
    let written = custom_engines_beside(&engines_beside(&document));
    assert_eq!(out["engines_file"], written.to_string_lossy().as_ref());
    let custom = EnginesFile::load(&written).unwrap();
    assert_eq!(custom.revision, 1);
    let profile = &custom.profiles["basal-serve"];
    let parsed = CustomManifest::parse(MANIFEST).unwrap();
    assert_eq!(profile["engine"], "custom");
    assert_eq!(profile["build_fingerprint"], "2.2.0");
    assert_eq!(
        profile["custom"]["manifest"],
        serde_json::to_value(&parsed).unwrap()
    );
    assert_eq!(profile["custom"]["manifest_digest"], parsed.digest());
    assert_eq!(out["manifest_digest"], parsed.digest());
    // T21 T22 (ADR 0031 §8): the installation digest the summary showed is
    // the one written, pinned for registration to compare against.
    let shown = out["fingerprint"]["digest"].as_str().unwrap();
    assert!(out["installation"].as_str().unwrap().starts_with(shown));
    assert_eq!(profile["custom"]["installation_digest"], shown);
    assert_eq!(profile["security"]["deep_park"], "disabled");
    assert!(
        !engines_beside(&document).exists(),
        "a custom registration never touches engines.yaml"
    );
    assert_eq!(std::fs::read(&document).unwrap(), host_before);
    assert_eq!(*role.0.lock().unwrap(), vec![ControlRequest::Add]);
    // The manifest file is read once: editing it later changes nothing.
    std::fs::write(&manifest, "garbage").unwrap();
    assert_eq!(EnginesFile::load(&written).unwrap(), custom);
    // ADR 0031 §3: the pinned digest covers the environment's dependencies.
    let measure = || {
        capyctl_agent::installation::InstallationMeasurer::new()
            .measure_profile(
                capyctl_config::engine_policy::Engine::Custom,
                &venv.executable,
                Some(&parsed),
            )
            .unwrap()
            .digest
    };
    assert_eq!(out["fingerprint"]["digest"], measure().as_str());
    std::fs::write(&venv.dependency, "VERSION = 2\n").unwrap();
    assert_ne!(out["fingerprint"]["digest"], measure().as_str());
}

// T44 (amendment A1): engines.yaml keeps 0.1.5's shape. A known engine's
// registration writes it; a custom registration beside it leaves its bytes
// unchanged, and the file still parses with the closed `ConfigKind::Engines`
// profile schema, which refuses a custom profile.
#[tokio::test]
async fn engines_yaml_stays_compatible_with_0_1_5() {
    let dir = private_dir();
    let marker = dir.path().join("ran");
    let venv = venv(dir.path(), &marker);
    let vllm = vllm_env(&dir.path().join("v"));
    let document = host_doc(dir.path());
    let (_role, _stop) = role(&document, json!({"ok": true, "published": "published"})).await;
    execute(&add_vllm(&vllm, None), Some(&document), dir.path())
        .await
        .unwrap();
    let engines = engines_beside(&document);
    let before = std::fs::read(&engines).unwrap();
    let manifest = manifest_file(dir.path(), MANIFEST);
    execute(
        &add_custom(&venv.executable, "basal-serve", &manifest),
        Some(&document),
        dir.path(),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(&engines).unwrap(), before);
    let text = String::from_utf8(before).unwrap();
    capyctl_config::parse_strict(capyctl_config::ConfigKind::Engines, &text).unwrap();
    let parsed = EnginesFile::load(&engines).unwrap();
    assert_eq!(parsed.profiles.keys().collect::<Vec<_>>(), vec!["vllm"]);
    // A custom profile in engines.yaml's shape is what 0.1.5 would refuse.
    let custom = EnginesFile::load(&custom_engines_beside(&engines)).unwrap();
    let mut mixed = serde_json::from_str::<Value>(
        text.lines()
            .filter(|l| !l.starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n")
            .as_str(),
    )
    .unwrap();
    mixed["runtime_profiles"]["basal-serve"] = custom.profiles["basal-serve"].clone();
    assert!(
        capyctl_config::parse_strict(capyctl_config::ConfigKind::Engines, &mixed.to_string())
            .is_err()
    );
}

// T44 T14 (ADR 0031 §1, §10): every refusal comes before anything runs or
// is written: `--deep-park enabled` (capability_missing), a missing or
// non-executable executable (engine_not_found, exit 16), an unknown or a
// missing required placeholder, a manifest too large or naming an
// `environment` that does not exist (custom_manifest_invalid, exit 27), a
// reserved environment name (engine_env_reserved), and a name registered in
// either file (profile_exists, exit 19).
#[tokio::test]
async fn add_custom_refuses_before_anything_runs() {
    let dir = private_dir();
    let marker = dir.path().join("ran");
    let venv = venv(dir.path(), &marker);
    let document = host_doc(dir.path());
    let good = manifest_file(dir.path(), MANIFEST);
    let run = |command: Command| {
        let (document, state) = (document.clone(), dir.path().to_path_buf());
        async move { execute(&command, Some(&document), &state).await }
    };
    let refused = |result: Result<Value, StructuredError>, code: &str, exit: i32| {
        let error = result.unwrap_err();
        assert_eq!(error.closed_code(), code, "{error:?}");
        assert_eq!(error.exit_code(), ExitCode(exit), "{error:?}");
    };

    let mut deep = add_custom(&venv.executable, "basal", &good);
    if let Command::EngineAddCustom { deep_park, .. } = &mut deep {
        *deep_park = Some(DeepParkChoice::Enabled);
    }
    refused(run(deep).await, "capability_missing", 5);

    let missing = dir.path().join("nowhere/serve");
    refused(
        run(add_custom(&missing, "basal", &good)).await,
        "engine_not_found",
        16,
    );
    let plain = dir.path().join("plain");
    std::fs::write(&plain, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
    refused(
        run(add_custom(&plain, "basal", &good)).await,
        "engine_not_found",
        16,
    );
    refused(
        run(add_custom(&venv.root, "basal", &good)).await,
        "engine_not_found",
        16,
    );

    for text in [
        MANIFEST.replace("{model_dir}", "{checkpoint}"),
        MANIFEST.replace("--model, \"{model_dir}\"", "--model, /srv/m"),
        MANIFEST.replace("health: /health", "health: /health/{port}"),
        format!("{MANIFEST}color: blue\n"),
        format!(
            "{MANIFEST}environment: {}\n",
            dir.path().join("absent").display()
        ),
        format!("{MANIFEST}# {}\n", "x".repeat(64 * 1024)),
    ] {
        let manifest = manifest_file(dir.path(), &text);
        refused(
            run(add_custom(&venv.executable, "basal", &manifest)).await,
            "custom_manifest_invalid",
            27,
        );
    }
    refused(
        run(add_custom(
            &venv.executable,
            "basal",
            &dir.path().join("no-manifest.yaml"),
        ))
        .await,
        "custom_manifest_invalid",
        27,
    );
    let reserved = manifest_file(dir.path(), &format!("{MANIFEST}env: {{PATH: /opt/bin}}\n"));
    refused(
        run(add_custom(&venv.executable, "basal", &reserved)).await,
        "engine_env_reserved:PATH",
        2,
    );
    let mut flagged = add_custom(&venv.executable, "basal", &good);
    if let Command::EngineAddCustom { env, .. } = &mut flagged {
        *env = vec![("LD_PRELOAD".into(), "/x.so".into())];
    }
    refused(run(flagged).await, "engine_env_reserved:LD_PRELOAD", 2);
    refused(
        run(add_custom(&venv.executable, "local", &good)).await,
        "profile_exists",
        19,
    );
    assert!(!marker.exists(), "nothing ran");
    let custom = custom_engines_beside(&engines_beside(&document));
    assert!(!custom.exists(), "nothing was written");

    // A name in one file is refused in the other, either way round.
    run(add_custom(&venv.executable, "basal", &good))
        .await
        .unwrap();
    let vllm = vllm_env(&dir.path().join("v"));
    refused(
        run(add_vllm(&vllm, Some("basal"))).await,
        "profile_exists",
        19,
    );
    run(add_vllm(&vllm, Some("vllm"))).await.unwrap();
    refused(
        run(add_custom(&venv.executable, "vllm", &good)).await,
        "profile_exists",
        19,
    );
    assert!(!marker.exists(), "nothing ran");
}

// T44 T37 (ADR 0031 §8): without a terminal and without `--yes`, `engine
// add --custom` prints what it would register and refuses `not_interactive`
// (exit 23); nothing is written and nothing runs.
#[test]
fn add_custom_without_a_terminal_needs_yes() {
    let dir = private_dir();
    let marker = dir.path().join("ran");
    let venv = venv(dir.path(), &marker);
    let document = host_doc(dir.path());
    let manifest = manifest_file(dir.path(), MANIFEST);
    let out = support::capyctl()
        .env("HOME", dir.path())
        .env("CAPYCTL_STATE_DIR", dir.path().join("state"))
        .args([
            "engine",
            "add",
            "--custom",
            venv.executable.to_str().unwrap(),
            "--name",
            "basal",
            "--manifest",
            manifest.to_str().unwrap(),
            "--config",
            document.to_str().unwrap(),
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(23), "{stderr}");
    assert!(stderr.contains("not_interactive"), "{stderr}");
    assert!(
        stderr.contains(&venv.executable.display().to_string()),
        "{stderr}"
    );
    assert!(stderr.contains("--mode eager --host {host}"), "{stderr}");
    assert!(
        stderr.contains("engine key:   none (the engine listens unauthenticated"),
        "{stderr}"
    );
    assert!(!marker.exists());
    assert!(!custom_engines_beside(&engines_beside(&document)).exists());
}

// T44 (ADR 0031 §1): `--custom` excludes the positional path and every flag
// that configures arguments CapyCTL renders for known engines, and needs
// `--name` and `--manifest`.
#[test]
fn the_custom_grammar_is_closed() {
    let base = [
        "capyctl",
        "engine",
        "add",
        "--custom",
        "/opt/serve/bin/serve",
    ];
    let with = |extra: &[&str]| parse(base.iter().chain(extra).copied().collect::<Vec<_>>());
    assert_eq!(
        with(&[
            "--name",
            "basal",
            "--manifest",
            "/m.yaml",
            "--yes",
            "--drift",
            "refuse"
        ])
        .unwrap(),
        Command::EngineAddCustom {
            executable: "/opt/serve/bin/serve".into(),
            name: "basal".into(),
            manifest: "/m.yaml".into(),
            deep_park: None,
            drift: DriftChoice::Refuse,
            env: vec![],
            approved_env: vec![],
            yes: true,
        }
    );
    assert!(
        with(&["--manifest", "/m.yaml"]).is_err(),
        "--name is required"
    );
    assert!(
        with(&["--name", "basal"]).is_err(),
        "--manifest is required"
    );
    for forbidden in [
        &["/venv"][..],
        &["--arg", "--x"],
        &["--approve-option", "--x"],
        &["--approve-path", "/srv"],
        &["--allow-remote-code"],
    ] {
        let mut args = vec!["--name", "basal", "--manifest", "/m.yaml"];
        args.extend_from_slice(forbidden);
        assert!(with(&args).is_err(), "{forbidden:?}");
    }
    assert!(parse(["capyctl", "engine", "add", "--manifest", "/m.yaml"]).is_err());
    assert!(parse(["capyctl", "engine", "add", "/venv", "--yes"]).is_err());
}

// T44 T37 (ADR 0031 §6, §7): `engine list` shows a custom engine with
// `CUSTOM yes`, the manifest's version, its idle signal and whether its
// launches carry a per-launch engine key; one without either reads `none`.
// A key placed in the arguments is warned about at registration. `engine
// remove` finds a custom profile in custom-engines.yaml.
#[tokio::test]
async fn list_shows_the_idle_signal_and_engine_key_and_remove_finds_it() {
    let dir = private_dir();
    let marker = dir.path().join("ran");
    let venv = venv(dir.path(), &marker);
    let document = host_doc(dir.path());
    let keyed = manifest_file(
        dir.path(),
        &format!(
            "{}env: {{SERVE_KEY: \"{{api_key}}\"}}\nidle:\n  url: /metrics\n  gauges: [requests_running, requests_waiting]\n",
            MANIFEST.replace("version: \"2.2.0\"\n", "")
        ),
    );
    let out = execute(
        &add_custom(&venv.executable, "keyed", &keyed),
        Some(&document),
        dir.path(),
    )
    .await
    .unwrap();
    assert_eq!(out["published"], "role_not_running");
    assert_eq!(out["engine_key"], "per-launch");
    assert!(out.get("warnings").is_none(), "{out}");
    let in_args = manifest_file(
        dir.path(),
        &MANIFEST.replace("--mode, eager", "--mode, eager, --auth, \"{api_key}\""),
    );
    let out = execute(
        &add_custom(&venv.executable, "in-args", &in_args),
        Some(&document),
        dir.path(),
    )
    .await
    .unwrap();
    assert!(
        out["warnings"][0]
            .as_str()
            .is_some_and(|w| w.contains("/proc/<pid>/cmdline")),
        "{out}"
    );
    let plain = manifest_file(dir.path(), MANIFEST);
    execute(
        &add_custom(&venv.executable, "plain", &plain),
        Some(&document),
        dir.path(),
    )
    .await
    .unwrap();

    let listed = execute(&Command::EngineList, Some(&document), dir.path())
        .await
        .unwrap();
    let row = |name: &str| {
        listed["engines"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["profile"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("{name} in {listed}"))
    };
    let keyed = row("keyed");
    assert_eq!(keyed["source"], "custom-engines.yaml");
    assert_eq!(keyed["engine"], "custom");
    assert_eq!(keyed["custom"], true);
    assert_eq!(keyed["version"], "unversioned");
    assert_eq!(
        keyed["idle_signal"],
        "/metrics gauges requests_running,requests_waiting"
    );
    assert_eq!(keyed["engine_key"], "per-launch");
    let plain = row("plain");
    assert_eq!(plain["version"], "2.2.0");
    assert_eq!(plain["idle_signal"], "none");
    assert_eq!(plain["engine_key"], "none");
    assert_eq!(plain["custom"], true);
    assert!(!marker.exists());

    let removed = execute(
        &Command::EngineRemove {
            name: "plain".into(),
            drain: false,
        },
        Some(&document),
        dir.path(),
    )
    .await
    .unwrap();
    let custom = custom_engines_beside(&engines_beside(&document));
    assert_eq!(removed["engines_file"], custom.to_string_lossy().as_ref());
    let left = EnginesFile::load(&custom).unwrap();
    assert!(!left.profiles.contains_key("plain"));
    assert!(left.profiles.contains_key("keyed"));
}

// T44 (ADR 0031 §1): `engine detect` never lists a custom engine; there is
// nothing to detect, and nothing runs.
#[tokio::test]
async fn detect_never_lists_custom_engines() {
    let dir = private_dir();
    let marker = dir.path().join("ran");
    venv(dir.path(), &marker);
    let found = execute(
        &Command::EngineDetect {
            paths: vec![dir.path().to_path_buf()],
        },
        None,
        dir.path(),
    )
    .await
    .unwrap();
    let candidates = found["candidates"].as_array().unwrap();
    assert!(
        candidates.iter().all(|c| c["engine"] != "custom"
            && !c["env"]
                .as_str()
                .is_some_and(|env| env.starts_with(&*dir.path().to_string_lossy()))),
        "{found}"
    );
    assert!(!marker.exists());
}
