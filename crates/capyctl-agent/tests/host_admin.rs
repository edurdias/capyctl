//! ADR 0040 §10, §11: host administration as a role's management routes
//! drive it: detection with extra search directories, a built-in add that
//! runs code only on a detected candidate or a confirmed preview, the custom
//! engine preview and confirmation over the confirmation digest, remove,
//! model copies and the effective configuration. Fake installations and an
//! in-process role channel only: CPU evidence, never qualification.
use capyctl_agent::control_socket::{ClientError, ControlRequest, SOCKET_NAME};
use capyctl_agent::engines::ScanRoots;
use capyctl_agent::host_admin::{
    self, BuiltinAdd, BuiltinApproval, ConfigLayers, CustomEngine, CustomOperation, ManifestInput,
    RoleChannel, RoleKind, Target, Toolchain, CONFIRMATION_REQUIRED, CONFIRMATION_STALE,
    MAX_REMOTE_MANIFEST_BYTES,
};
use capyctl_agent::installation::InstallationMeasurer;
use capyctl_config::custom::MANIFEST_INVALID;
use capyctl_config::effective::InstallationDrift;
use capyctl_config::registration::{custom_engines_beside, engines_beside, EnginesFile};
use capyctl_config::setting_overrides::{NamedLayer, SettingOverrides};
use capyctl_config::toolchain::ToolchainSearch;
use capyctl_config::ConfigKind;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A role answering every control request with `reply`, recording what it
/// was asked: the in-process channel a role's management routes use.
struct Role {
    asked: Mutex<Vec<ControlRequest>>,
    reply: Value,
}

impl Role {
    fn answering(reply: Value) -> Self {
        Self {
            asked: Mutex::new(vec![]),
            reply,
        }
    }
    fn published() -> Self {
        Self::answering(json!({"ok": true, "published": true}))
    }
    fn asked(&self) -> usize {
        self.asked.lock().unwrap().len()
    }
}

impl RoleChannel for Role {
    async fn ask(&self, request: ControlRequest, _: Duration) -> Result<Value, ClientError> {
        self.asked.lock().unwrap().push(request);
        Ok(self.reply.clone())
    }
}

fn script(path: &Path, body: &str) {
    capyctl_config::test_support::write_executable(path, format!("#!/bin/sh\n{body}\n"), 0o755)
        .unwrap();
}

/// A private directory under HOME: custom executables in a world-writable
/// directory are refused (ADR 0031 §10).
fn private_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

/// A host whose document and engines files live in `dir`.
fn host(dir: &Path) -> Target {
    let state = dir.join("s");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
    let document = dir.join("host.yaml");
    std::fs::write(
        &document,
        capyctl_config::remote_roles::HostConfig::template(&state),
    )
    .unwrap();
    Target {
        engines: engines_beside(&document),
        role_document: document,
        kind: RoleKind::Host,
        socket: state.join(SOCKET_NAME),
        state_dir: state,
    }
}

/// Toolchain directories of the test's own, so the machine does not matter.
fn search(dir: &Path) -> ToolchainSearch {
    ToolchainSearch {
        system: dir.join("no-bin").display().to_string(),
        default_cuda_home: dir.join("no-cuda"),
        system_root: dir.join("no-root"),
    }
}

/// Detection roots that see nothing of this machine's HOME or PATH.
fn roots(home: &Path, extra: Vec<PathBuf>) -> ScanRoots {
    ScanRoots {
        path_dirs: vec![],
        home: Some(home.to_path_buf()),
        xdg_data: Some(home.join(".local/share")),
        pipx_home: None,
        conda_roots: vec![],
        opt: None,
        system_bins: vec![],
        extra,
    }
}

/// A vLLM venv whose `vllm --version` prints the version and whose
/// interpreter answers the capability probe.
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

fn builtin(path: &Path, remote_code: bool) -> BuiltinAdd {
    BuiltinAdd {
        host_id: "h1".into(),
        path: path.to_path_buf(),
        name: None,
        deep_park: None,
        drift: InstallationDrift::Warn,
        args: vec![],
        approved_options: vec![],
        approved_paths: vec![],
        env: BTreeMap::new(),
        approved_env: vec![],
        remote_code,
    }
}

fn engines_profiles(target: &Target) -> serde_json::Map<String, Value> {
    EnginesFile::load(&target.engines).unwrap().profiles
}

fn custom_profiles(target: &Target) -> serde_json::Map<String, Value> {
    EnginesFile::load(&custom_engines_beside(&target.engines))
        .unwrap()
        .profiles
}

// ADR 0040 §10: an installation outside the role's HOME and PATH is found
// only when its directory is passed as an extra search directory.
#[test]
fn detection_finds_an_installation_only_under_an_extra_directory() {
    let dir = private_dir();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let elsewhere = dir.path().join("elsewhere");
    let env = vllm_env(&elsewhere.join("vllm-env"));
    let listed = |roots: &ScanRoots| {
        host_admin::detected(roots)["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["env"] == json!(env) && c["engine"] == "vllm")
    };
    assert!(!listed(&roots(&home, vec![])));
    assert!(listed(&roots(&home, vec![elsewhere])));
    // KTD11: PATH candidates use the same detection as extra directories.
    // Supply the role's PATH snapshot without mutating the test process.
    let mut on_path = roots(&home, vec![]);
    on_path.path_dirs = vec![env.join("bin")];
    assert!(listed(&on_path));
}

// ADR 0040 §10: a remote built-in add runs a detected candidate; one naming
// an executable path detection does not list, or allowing remote code, is
// refused without a previewed digest, and nothing is written or asked.
#[tokio::test]
async fn a_builtin_add_runs_code_only_on_a_detected_candidate() {
    let dir = private_dir();
    let target = host(dir.path());
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let elsewhere = dir.path().join("elsewhere");
    let env = vllm_env(&elsewhere.join("vllm-env"));
    let search = search(dir.path());
    let toolchain = Toolchain {
        cuda_home_env: None,
        search: &search,
    };
    let measurer = Arc::new(InstallationMeasurer::new());
    let role = Role::published();

    let undetected = roots(&home, vec![]);
    let refused = host_admin::add_builtin(
        &target,
        &builtin(&env.join("bin/vllm"), false),
        &toolchain,
        BuiltinApproval::Detected(&undetected),
        &measurer,
        &role,
    )
    .await
    .unwrap_err();
    assert_eq!(refused.code, CONFIRMATION_REQUIRED, "{refused}");

    let detected = roots(&home, vec![elsewhere]);
    let refused = host_admin::add_builtin(
        &target,
        &builtin(&env, true),
        &toolchain,
        BuiltinApproval::Detected(&detected),
        &measurer,
        &role,
    )
    .await
    .unwrap_err();
    assert_eq!(refused.code, CONFIRMATION_REQUIRED, "{refused}");
    assert!(engines_profiles(&target).is_empty());
    assert_eq!(role.asked(), 0);

    // ADR 0040 §10 / KTD11 / T18: caller-supplied discovery roots do
    // not authorize execution, even when they reveal this installation.
    let refused = host_admin::add_builtin(
        &target,
        &builtin(&env, false),
        &toolchain,
        BuiltinApproval::Detected(&detected),
        &measurer,
        &role,
    )
    .await
    .unwrap_err();
    assert_eq!(refused.code, CONFIRMATION_REQUIRED);
    assert!(engines_profiles(&target).is_empty());
    assert_eq!(role.asked(), 0);

    // The host's fixed PATH snapshot may authorize the same candidate.
    let mut fixed = roots(&home, vec![]);
    fixed.path_dirs = vec![env.join("bin")];
    let added = host_admin::add_builtin(
        &target,
        &builtin(&env, false),
        &toolchain,
        BuiltinApproval::Detected(&fixed),
        &measurer,
        &role,
    )
    .await
    .unwrap();
    assert_eq!(added["profile"], "vllm");
    assert_eq!(added["published"], true);
    assert!(engines_profiles(&target).contains_key("vllm"));
    assert_eq!(role.asked(), 1);
}

// ADR 0040 §10, §11: a built-in add allowing remote code goes through the
// preview: the digest and the typed profile name confirm it; a wrong name
// is refused.
#[tokio::test]
async fn a_builtin_add_with_remote_code_is_confirmed_against_its_preview() {
    let dir = private_dir();
    let target = host(dir.path());
    let env = vllm_env(&dir.path().join("vllm-env"));
    let search = search(dir.path());
    let toolchain = Toolchain {
        cuda_home_env: None,
        search: &search,
    };
    let measurer = Arc::new(InstallationMeasurer::new());
    let role = Role::published();
    let request = builtin(&env, true);
    let preview = host_admin::preview_builtin(&target, &request, &toolchain, &measurer)
        .await
        .unwrap();
    assert!(preview.digest.starts_with("sha256:"));
    assert!(preview.summary.contains("remote code:   allowed"));
    assert_eq!(
        preview.command[0],
        env.join("bin/vllm").display().to_string()
    );

    let refused = host_admin::add_builtin(
        &target,
        &request,
        &toolchain,
        BuiltinApproval::Confirmed {
            digest: &preview.digest,
            typed_name: "vllm2",
        },
        &measurer,
        &role,
    )
    .await
    .unwrap_err();
    assert_eq!(refused.code, CONFIRMATION_STALE, "{refused}");
    assert!(engines_profiles(&target).is_empty());

    let added = host_admin::add_builtin(
        &target,
        &request,
        &toolchain,
        BuiltinApproval::Confirmed {
            digest: &preview.digest,
            typed_name: "vllm",
        },
        &measurer,
        &role,
    )
    .await
    .unwrap();
    assert_eq!(added["trust_remote_code"], true);
    assert!(engines_profiles(&target).contains_key("vllm"));
}

/// A virtual environment with a console script `bin/serve` that touches
/// `executed` if it ever runs, and a dependency in `site-packages`.
struct Venv {
    executable: PathBuf,
    dependency: PathBuf,
    executed: PathBuf,
}

fn venv(dir: &Path) -> Venv {
    let root = dir.join("serve-venv");
    let site = root.join("lib/python3.12/site-packages/serve_dep");
    std::fs::create_dir_all(root.join("bin")).unwrap();
    std::fs::create_dir_all(&site).unwrap();
    std::fs::write(root.join("pyvenv.cfg"), "home = /usr/bin\n").unwrap();
    let dependency = site.join("__init__.py");
    std::fs::write(&dependency, "VERSION = 1\n").unwrap();
    let executed = dir.join("executed");
    let executable = root.join("bin/serve");
    script(&executable, &format!("touch {}", executed.display()));
    Venv {
        executable,
        dependency,
        executed,
    }
}

const MANIFEST: &str = r#"kind: custom_engine
schema_version: 1
api: decisions
version: "2.2.0"
args: [--mode, eager, --host, "{host}", --port, "{port}", --model, "{model_dir}"]
health: /health
"#;

fn custom(venv: &Venv, name: &str, operation: CustomOperation) -> CustomEngine {
    CustomEngine {
        host_id: "h1".into(),
        operation,
        name: name.into(),
        executable: venv.executable.clone(),
        manifest: ManifestInput::Remote(MANIFEST.into()),
        deep_park: None,
        drift: InstallationDrift::Warn,
        env: BTreeMap::from([("SERVE_MODE".to_owned(), "fast".to_owned())]),
        approved_env: vec![],
    }
}

// AE6, ADR 0040 §11: the preview returns the command, the approval summary
// and a digest; confirming with that digest and the profile name pins the
// previewed installation digest and asks the role to publish. Nothing runs.
#[tokio::test]
async fn a_custom_preview_is_confirmed_and_pins_the_installation() {
    let dir = private_dir();
    let target = host(dir.path());
    let venv = venv(dir.path());
    let measurer = Arc::new(InstallationMeasurer::new());
    let role = Role::published();
    let request = custom(&venv, "serve", CustomOperation::Add);
    let preview = host_admin::preview_custom(&target, &request, &measurer)
        .await
        .unwrap();
    assert_eq!(
        preview.command,
        [venv.executable.display().to_string()]
            .into_iter()
            .chain(
                [
                    "--mode",
                    "eager",
                    "--host",
                    "{host}",
                    "--port",
                    "{port}",
                    "--model",
                    "{model_dir}"
                ]
                .map(String::from)
            )
            .collect::<Vec<_>>()
    );
    assert!(preview.summary.starts_with("custom engine serve\n"));
    assert!(preview.digest.starts_with("sha256:") && preview.digest.len() == 71);
    assert!(
        custom_profiles(&target).is_empty(),
        "a preview writes nothing"
    );

    let out = host_admin::confirm_custom(
        &target,
        &request,
        &preview.digest,
        "serve",
        &measurer,
        &role,
    )
    .await
    .unwrap();
    assert_eq!(out["published"], true);
    let pinned = out["fingerprint"]["digest"].as_str().unwrap().to_owned();
    assert!(preview.summary.contains(&pinned));
    assert_eq!(
        custom_profiles(&target)["serve"]["custom"]["installation_digest"],
        json!(pinned)
    );
    assert_eq!(role.asked(), 1);
    assert!(!venv.executed.exists(), "nothing runs");
}

// ADR 0040 §11: a file of the installation changed after the preview.
#[tokio::test]
async fn confirming_after_an_installation_file_changed_is_stale() {
    let dir = private_dir();
    let target = host(dir.path());
    let venv = venv(dir.path());
    let measurer = Arc::new(InstallationMeasurer::new());
    let role = Role::published();
    let request = custom(&venv, "serve", CustomOperation::Add);
    let preview = host_admin::preview_custom(&target, &request, &measurer)
        .await
        .unwrap();
    std::fs::write(&venv.dependency, "VERSION = 2\n").unwrap();
    let refused = host_admin::confirm_custom(
        &target,
        &request,
        &preview.digest,
        "serve",
        &measurer,
        &role,
    )
    .await
    .unwrap_err();
    assert_eq!(refused.code, CONFIRMATION_STALE, "{refused}");
    assert!(custom_profiles(&target).is_empty());
    assert_eq!(role.asked(), 0);
}

// ADR 0040 §11 / KTD11 / T18: disappearance invalidates the preview,
// rather than returning the fresh-request executable validation error.
#[tokio::test]
async fn confirming_after_the_executable_disappeared_is_stale() {
    let dir = private_dir();
    let target = host(dir.path());
    let venv = venv(dir.path());
    let measurer = Arc::new(InstallationMeasurer::new());
    let role = Role::published();
    let request = custom(&venv, "serve", CustomOperation::Add);
    let preview = host_admin::preview_custom(&target, &request, &measurer)
        .await
        .unwrap();
    std::fs::remove_file(&request.executable).unwrap();
    let refused = host_admin::confirm_custom(
        &target,
        &request,
        &preview.digest,
        "serve",
        &measurer,
        &role,
    )
    .await
    .unwrap_err();
    assert_eq!(refused.code, CONFIRMATION_STALE);
    assert!(custom_profiles(&target).is_empty());
    assert_eq!(role.asked(), 0);
}

// ADR 0040 §11 / KTD11 / T18: a removed custom profile cannot be
// recreated by confirming an earlier change preview.
#[tokio::test]
async fn confirming_after_the_custom_profile_disappeared_is_stale() {
    let dir = private_dir();
    let target = host(dir.path());
    let venv = venv(dir.path());
    let measurer = Arc::new(InstallationMeasurer::new());
    let role = Role::published();
    let add = custom(&venv, "serve", CustomOperation::Add);
    let preview = host_admin::preview_custom(&target, &add, &measurer)
        .await
        .unwrap();
    host_admin::confirm_custom(&target, &add, &preview.digest, "serve", &measurer, &role)
        .await
        .unwrap();
    let change = custom(&venv, "serve", CustomOperation::Change);
    let preview = host_admin::preview_custom(&target, &change, &measurer)
        .await
        .unwrap();
    std::fs::remove_file(custom_engines_beside(&target.engines)).unwrap();
    let asked = role.asked();
    let refused =
        host_admin::confirm_custom(&target, &change, &preview.digest, "serve", &measurer, &role)
            .await
            .unwrap_err();
    assert_eq!(refused.code, CONFIRMATION_STALE);
    assert!(custom_profiles(&target).is_empty());
    assert_eq!(role.asked(), asked);
}

// ADR 0040 §11 / KTD11 / T18: compare the installation digest before
// reporting a registry conflict introduced after the preview.
#[tokio::test]
async fn stale_custom_digest_precedes_a_new_profile_conflict() {
    let dir = private_dir();
    let target = host(dir.path());
    let venv = venv(dir.path());
    let measurer = Arc::new(InstallationMeasurer::new());
    let role = Role::published();
    let request = custom(&venv, "serve", CustomOperation::Add);
    let preview = host_admin::preview_custom(&target, &request, &measurer)
        .await
        .unwrap();
    host_admin::confirm_custom(
        &target,
        &request,
        &preview.digest,
        "serve",
        &measurer,
        &role,
    )
    .await
    .unwrap();
    std::fs::write(&venv.dependency, "VERSION = 2\n").unwrap();
    let asked = role.asked();
    let refused = host_admin::confirm_custom(
        &target,
        &request,
        &preview.digest,
        "serve",
        &measurer,
        &role,
    )
    .await
    .unwrap_err();
    assert_eq!(refused.code, CONFIRMATION_STALE);
    assert_eq!(role.asked(), asked);
    assert!(custom_profiles(&target).contains_key("serve"));
}

// ADR 0040 §11: the profile's environment value changed after the preview.
#[tokio::test]
async fn confirming_with_a_changed_environment_value_is_stale() {
    let dir = private_dir();
    let target = host(dir.path());
    let venv = venv(dir.path());
    let measurer = Arc::new(InstallationMeasurer::new());
    let role = Role::published();
    let request = custom(&venv, "serve", CustomOperation::Add);
    let preview = host_admin::preview_custom(&target, &request, &measurer)
        .await
        .unwrap();
    let mut changed = request.clone();
    changed.env.insert("SERVE_MODE".into(), "slow".into());
    let refused = host_admin::confirm_custom(
        &target,
        &changed,
        &preview.digest,
        "serve",
        &measurer,
        &role,
    )
    .await
    .unwrap_err();
    assert_eq!(refused.code, CONFIRMATION_STALE, "{refused}");
    assert!(custom_profiles(&target).is_empty());
}

// ADR 0040 §11: a change previewed at one custom-engines.yaml revision is
// refused once the file moved to a newer one.
#[tokio::test]
async fn confirming_a_change_after_the_custom_engines_revision_moved_is_stale() {
    let dir = private_dir();
    let target = host(dir.path());
    let venv = venv(dir.path());
    let measurer = Arc::new(InstallationMeasurer::new());
    let role = Role::published();
    let add = custom(&venv, "serve", CustomOperation::Add);
    let preview = host_admin::preview_custom(&target, &add, &measurer)
        .await
        .unwrap();
    host_admin::confirm_custom(&target, &add, &preview.digest, "serve", &measurer, &role)
        .await
        .unwrap();
    let mut change = custom(&venv, "serve", CustomOperation::Change);
    change.drift = InstallationDrift::Refuse;
    let preview = host_admin::preview_custom(&target, &change, &measurer)
        .await
        .unwrap();
    // Another profile is written meanwhile: the revision moves.
    let other = custom(&venv, "other", CustomOperation::Add);
    let other_preview = host_admin::preview_custom(&target, &other, &measurer)
        .await
        .unwrap();
    host_admin::confirm_custom(
        &target,
        &other,
        &other_preview.digest,
        "other",
        &measurer,
        &role,
    )
    .await
    .unwrap();
    let refused =
        host_admin::confirm_custom(&target, &change, &preview.digest, "serve", &measurer, &role)
            .await
            .unwrap_err();
    assert_eq!(refused.code, CONFIRMATION_STALE, "{refused}");
    assert!(
        custom_profiles(&target)["serve"]["security"]
            .get("installation_drift")
            .is_none(),
        "the change was not written"
    );

    // Previewed again at the current revision, the change is confirmed.
    let preview = host_admin::preview_custom(&target, &change, &measurer)
        .await
        .unwrap();
    host_admin::confirm_custom(&target, &change, &preview.digest, "serve", &measurer, &role)
        .await
        .unwrap();
    assert_eq!(
        custom_profiles(&target)["serve"]["security"]["installation_drift"],
        "refuse"
    );
}

// ADR 0040 §11: the typed confirmation is the profile name.
#[tokio::test]
async fn confirming_with_the_wrong_typed_name_is_refused() {
    let dir = private_dir();
    let target = host(dir.path());
    let venv = venv(dir.path());
    let measurer = Arc::new(InstallationMeasurer::new());
    let role = Role::published();
    let request = custom(&venv, "serve", CustomOperation::Add);
    let preview = host_admin::preview_custom(&target, &request, &measurer)
        .await
        .unwrap();
    let refused = host_admin::confirm_custom(
        &target,
        &request,
        &preview.digest,
        "server",
        &measurer,
        &role,
    )
    .await
    .unwrap_err();
    assert_eq!(refused.code, CONFIRMATION_STALE, "{refused}");
    assert!(custom_profiles(&target).is_empty());
    assert_eq!(role.asked(), 0);
}

// ADR 0040 §11: a manifest sent to a host is capped below the 64 KiB
// server-to-host message limit.
#[tokio::test]
async fn a_manifest_above_the_cap_is_refused() {
    let dir = private_dir();
    let target = host(dir.path());
    let venv = venv(dir.path());
    let measurer = Arc::new(InstallationMeasurer::new());
    let mut request = custom(&venv, "serve", CustomOperation::Add);
    let padding = format!("# {}\n", "x".repeat(MAX_REMOTE_MANIFEST_BYTES));
    request.manifest = ManifestInput::Remote(format!("{MANIFEST}{padding}"));
    let refused = host_admin::preview_custom(&target, &request, &measurer)
        .await
        .unwrap_err();
    assert_eq!(refused.code, MANIFEST_INVALID, "{refused}");
    // The same manifest within the cap is previewed.
    request.manifest = ManifestInput::Remote(format!("{MANIFEST}# short\n"));
    host_admin::preview_custom(&target, &request, &measurer)
        .await
        .unwrap();
}

// ADR 0018 §4: removing a profile a deployment uses is refused with the
// role's code, and the profile stays registered.
#[tokio::test]
async fn removing_a_profile_in_use_is_refused_with_the_roles_code() {
    let dir = private_dir();
    let target = host(dir.path());
    let venv = venv(dir.path());
    let measurer = Arc::new(InstallationMeasurer::new());
    let request = custom(&venv, "serve", CustomOperation::Add);
    let preview = host_admin::preview_custom(&target, &request, &measurer)
        .await
        .unwrap();
    host_admin::confirm_custom(
        &target,
        &request,
        &preview.digest,
        "serve",
        &measurer,
        &Role::published(),
    )
    .await
    .unwrap();
    let busy = Role::answering(json!({"ok": false, "code": "profile_in_use",
        "message": "serve is in use", "deployments": ["chat"]}));
    let refused = host_admin::remove(&target, "serve", false, &busy)
        .await
        .unwrap_err();
    assert_eq!(refused.code, "profile_in_use");
    assert!(refused.message.ends_with(": chat"), "{refused}");
    assert!(custom_profiles(&target).contains_key("serve"));
}

/// A verified copy at `key` (`sources/…`) of `bytes`, as a download leaves it.
fn copy(store: &Path, key: &str, bytes: u64) {
    std::fs::create_dir_all(store.join(key)).unwrap();
    std::fs::write(
        store.join(key).join("weights.bin"),
        vec![0_u8; bytes as usize],
    )
    .unwrap();
    let state = store.join("sources/.capyctl");
    std::fs::create_dir_all(&state).unwrap();
    let id = hex::encode(&Sha256::digest(key.as_bytes())[..12]);
    std::fs::write(
        state.join(format!("{id}.verified")),
        json!({"key": key, "state": "verified", "bytes": bytes, "files": 1}).to_string(),
    )
    .unwrap();
}

// SPEC §6.3, ADR 0040 §10: listing marks the copies the reference set names;
// prune deletes only the unreferenced ones, and only when applied.
#[test]
fn listing_marks_referenced_copies_and_prune_removes_only_the_others() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("models");
    copy(&store, "sources/http/kept", 10);
    copy(&store, "sources/http/unused", 20);
    let referenced = BTreeSet::from(["sources/http/kept".to_owned()]);

    let listed = host_admin::list_copies(&store, &referenced).unwrap();
    assert_eq!(
        listed["copies"],
        json!([
            {"key": "sources/http/kept", "bytes": 10, "referenced": true},
            {"key": "sources/http/unused", "bytes": 20, "referenced": false},
        ])
    );

    let dry = host_admin::prune_copies(&store, &referenced, false).unwrap();
    assert_eq!(dry["removed_bytes"], 20);
    assert!(store.join("sources/http/unused").is_dir(), "not applied");

    let applied = host_admin::prune_copies(&store, &referenced, true).unwrap();
    assert_eq!(applied["applied"], true);
    assert_eq!(applied["removed"][0]["key"], "sources/http/unused");
    assert!(!store.join("sources/http/unused").exists());
    assert!(store.join("sources/http/kept").is_dir());
    assert_eq!(
        host_admin::list_copies(&store, &referenced).unwrap()["copies"],
        json!([{"key": "sources/http/kept", "bytes": 10, "referenced": true}])
    );
}

// Owner decision 2026-09-25, ADR 0040 §10: the host's effective
// configuration names each value's source; a stated value is `yaml`.
#[test]
fn host_effective_config_names_each_values_source() {
    let dir = private_dir();
    let target = host(dir.path());
    let text = std::fs::read_to_string(&target.role_document).unwrap();
    let stated = capyctl_config::parse_document(&text).unwrap();
    let none = NamedLayer::default();
    let overrides = SettingOverrides::none(ConfigKind::Host);
    let config = host_admin::effective_config(
        ConfigKind::Host,
        &target.role_document,
        Some(&stated),
        ConfigLayers {
            env: &none,
            flags: &none,
            overrides: &overrides,
        },
        Some(dir.path()),
        &target.state_dir,
    );
    assert_eq!(config["role"], "host");
    assert_eq!(
        config["document"],
        json!(target.role_document.display().to_string())
    );
    let settings = config["settings"].as_array().unwrap();
    assert!(settings
        .iter()
        .any(|s| s["path"] == "name" && s["value"] == "capyctl-host" && s["source"] == "yaml"));
}
