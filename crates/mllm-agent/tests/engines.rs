//! ADR 0018 §1: resolving a named installation and its bounded version
//! check. Fake environments only: CPU evidence, never qualification.
use mllm_agent::engines::*;
use mllm_config::engine_policy::Engine;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A venv-shaped tree: `bin/`, `lib/python3.12/site-packages/<pkg>-<v>.dist-info`.
pub fn fake_env(root: &Path, packages: &[(&str, &str)]) -> PathBuf {
    let site = root.join("lib/python3.12/site-packages");
    std::fs::create_dir_all(&site).unwrap();
    std::fs::create_dir_all(root.join("bin")).unwrap();
    std::fs::write(root.join("pyvenv.cfg"), "home = /usr/bin\n").unwrap();
    for (name, version) in packages {
        let info = site.join(format!("{name}-{version}.dist-info"));
        std::fs::create_dir_all(&info).unwrap();
        std::fs::write(
            info.join("METADATA"),
            format!("Metadata-Version: 2.1\nName: {name}\nVersion: {version}\n"),
        )
        .unwrap();
        std::fs::create_dir_all(site.join(name)).unwrap();
    }
    root.to_path_buf()
}

pub fn script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

// T07 T22: a venv directory, its bin/vllm and its bin/python3 all resolve to
// the same environment; the engine and version come from dist-info.
#[test]
fn a_named_path_resolves_to_its_environment() {
    let dir = tempfile::tempdir().unwrap();
    let env = fake_env(&dir.path().join("v"), &[("vllm", "0.29.0")]);
    script(&env.join("bin/vllm"), "echo 0.29.0");
    for named in [env.clone(), env.join("bin/vllm")] {
        let resolved = resolve(&named).unwrap();
        assert_eq!(resolved.engine, Engine::Vllm);
        assert_eq!(resolved.version, "0.29.0");
        assert_eq!(resolved.env, env);
        assert_eq!(resolved.executable, env.join("bin/vllm"));
        assert!(!resolved.custom());
    }
    let sg = fake_env(&dir.path().join("s"), &[("sglang", "0.5.20+custom")]);
    script(&sg.join("bin/python3"), "echo 0.5.20+custom");
    let resolved = resolve(&sg.join("bin/python3")).unwrap();
    assert_eq!(resolved.engine, Engine::Sglang);
    assert_eq!(resolved.executable, sg.join("bin/python3"));
    assert!(resolved.custom());
}

// T37 (Review Focus 2): a venv's python3 is a symlink to the system
// interpreter; resolution stays in the venv.
#[test]
fn the_interpreter_symlink_is_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    let system = fake_env(&dir.path().join("system"), &[("vllm", "0.1.0")]);
    script(&system.join("bin/python3"), "echo 0.1.0");
    let env = fake_env(&dir.path().join("venv"), &[("sglang", "0.5.20")]);
    std::os::unix::fs::symlink(system.join("bin/python3"), env.join("bin/python3")).unwrap();
    let resolved = resolve(&env.join("bin/python3")).unwrap();
    assert_eq!(resolved.env, env);
    assert_eq!(resolved.engine, Engine::Sglang);
    assert_eq!(resolved.executable, env.join("bin/python3"));
}

// T07: no engine package, not an environment, and ambiguity are named.
#[test]
fn unresolvable_paths_are_refused_with_their_code() {
    let dir = tempfile::tempdir().unwrap();
    let empty = fake_env(&dir.path().join("e"), &[("numpy", "2.0.0")]);
    assert_eq!(resolve(&empty).unwrap_err().code(), "engine_not_found");
    std::fs::create_dir_all(dir.path().join("plain")).unwrap();
    assert_eq!(
        resolve(&dir.path().join("plain")).unwrap_err().code(),
        "engine_unsupported"
    );
    let both = fake_env(
        &dir.path().join("b"),
        &[("vllm", "0.29.0"), ("sglang", "0.5.20")],
    );
    script(&both.join("bin/vllm"), "echo 0.29.0");
    script(&both.join("bin/python3"), "echo 0.5.20");
    let error = resolve(&both).unwrap_err();
    assert_eq!(error.code(), "engine_unsupported");
    assert!(error.to_string().contains("bin/vllm"), "{error}");
    assert_eq!(
        resolve(&both.join("bin/vllm")).unwrap().engine,
        Engine::Vllm
    );
    assert_eq!(
        resolve(&both.join("bin/python3")).unwrap().engine,
        Engine::Sglang
    );
    assert_eq!(
        resolve(&dir.path().join("b/../b")).unwrap_err().code(),
        "engine_unsupported"
    );
}

// T37: the version check is bounded, sees no inherited environment, and
// must agree with dist-info.
#[test]
fn the_version_check_is_bounded_and_must_agree() {
    let dir = tempfile::tempdir().unwrap();
    let env = fake_env(&dir.path().join("v"), &[("vllm", "0.29.0")]);
    script(&env.join("bin/vllm"), "echo 0.29.0");
    let resolved = resolve(&env).unwrap();
    assert_eq!(
        check_version(&resolved, Duration::from_secs(5)).unwrap(),
        "0.29.0"
    );

    std::env::set_var("MLLM_TEST_LEAK", "leaked");
    script(&env.join("bin/vllm"), "echo \"${MLLM_TEST_LEAK:-0.29.0}\"");
    assert_eq!(
        check_version(&resolved, Duration::from_secs(5)).unwrap(),
        "0.29.0"
    );

    script(&env.join("bin/vllm"), "echo 0.28.0");
    assert!(matches!(
        check_version(&resolved, Duration::from_secs(5)),
        Err(VersionCheckError::Mismatch { .. })
    ));
    script(&env.join("bin/vllm"), "exit 3");
    assert!(matches!(
        check_version(&resolved, Duration::from_secs(5)),
        Err(VersionCheckError::Failed)
    ));
    script(&env.join("bin/vllm"), "sleep 5; echo 0.29.0");
    let started = std::time::Instant::now();
    assert!(matches!(
        check_version(&resolved, Duration::from_millis(300)),
        Err(VersionCheckError::TimedOut)
    ));
    assert!(started.elapsed() < Duration::from_secs(3));
    script(&env.join("bin/vllm"), "yes 0.29.0 | head -c 100000");
    assert!(matches!(
        check_version(&resolved, Duration::from_secs(5)),
        Err(VersionCheckError::Output)
    ));
}
