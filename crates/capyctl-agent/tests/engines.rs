//! ADR 0018 §1: resolving a named installation and its bounded version
//! check. Fake environments only: CPU evidence, never qualification.
use capyctl_agent::engines::*;
use capyctl_config::engine_policy::Engine;
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

    std::env::set_var("CAPYCTL_TEST_LEAK", "leaked");
    script(
        &env.join("bin/vllm"),
        "echo \"${CAPYCTL_TEST_LEAK:-0.29.0}\"",
    );
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

fn empty_roots(home: &Path) -> ScanRoots {
    ScanRoots {
        path_dirs: vec![],
        home: Some(home.to_path_buf()),
        xdg_data: Some(home.join(".local/share")),
        pipx_home: None,
        conda_roots: vec![home.join("miniconda3")],
        opt: None,
        extra: vec![],
    }
}

// T07 T37: every documented location is found, and detection runs nothing.
#[test]
fn detection_finds_the_documented_locations_and_runs_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let marker = dir.path().join("executed");
    let run_marker = format!("touch {}; echo 0.29.0", marker.display());
    let venvs = fake_env(&home.join("venvs/a"), &[("vllm", "0.29.0")]);
    script(&venvs.join("bin/vllm"), &run_marker);
    let dot = fake_env(&home.join(".venv"), &[("sglang", "0.5.20")]);
    script(&dot.join("bin/python3"), &run_marker);
    let conda = fake_env(&home.join("miniconda3/envs/sg"), &[("sglang", "0.5.21")]);
    script(&conda.join("bin/python3"), &run_marker);
    let listed = fake_env(&dir.path().join("elsewhere/env"), &[("vllm", "0.30.0")]);
    script(&listed.join("bin/vllm"), &run_marker);
    std::fs::create_dir_all(home.join(".conda")).unwrap();
    std::fs::write(
        home.join(".conda/environments.txt"),
        format!("{}\n", listed.display()),
    )
    .unwrap();
    let uv = fake_env(
        &home.join(".local/share/uv/tools/vllm"),
        &[("vllm", "0.29.0")],
    );
    script(&uv.join("bin/vllm"), &run_marker);
    let on_path = fake_env(&dir.path().join("pathenv"), &[("vllm", "0.29.0")]);
    script(&on_path.join("bin/vllm"), &run_marker);
    let mut roots = empty_roots(&home);
    roots.path_dirs = vec![on_path.join("bin"), PathBuf::from("/nonexistent/bin")];
    let found = detect(&roots, &ScanBounds::default());
    let envs: std::collections::BTreeSet<_> = found.iter().map(|c| c.env.clone()).collect();
    for env in [&venvs, &dot, &conda, &listed, &uv, &on_path] {
        assert!(
            envs.contains(env),
            "{} missing from {found:?}",
            env.display()
        );
    }
    let custom: Vec<_> = found
        .iter()
        .filter(|c| c.custom)
        .map(|c| c.version.clone())
        .collect();
    assert!(custom.contains(&"0.5.21".to_string()) && custom.contains(&"0.30.0".to_string()));
    assert!(!marker.exists(), "detection executed an installation");
}

// T07 T37 (owner decision 2026-09-25): environments directly in the home
// directory (the Sparks' `~/capyctl-vllm-venv2` layout) are found without
// `--path`, one level deep and only when they carry `pyvenv.cfg`.
#[test]
fn home_level_environments_are_found_without_a_path() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let venv = fake_env(&home.join("capyctl-vllm-venv2"), &[("vllm", "0.29.0")]);
    script(&venv.join("bin/vllm"), "echo 0.29.0");
    let bare = fake_env(&home.join("not-a-venv"), &[("sglang", "0.5.20")]);
    std::fs::remove_file(bare.join("pyvenv.cfg")).unwrap();
    script(&bare.join("bin/python3"), "echo 0.5.20");
    let deeper = fake_env(&home.join("projects/env"), &[("vllm", "0.29.0")]);
    script(&deeper.join("bin/vllm"), "echo 0.29.0");
    let found = detect(&empty_roots(&home), &ScanBounds::default());
    let envs: Vec<_> = found.iter().map(|c| c.env.clone()).collect();
    assert!(envs.contains(&venv), "{found:?}");
    assert!(found.iter().any(|c| c.env == venv && c.source == "home"));
    assert!(
        !envs.contains(&bare),
        "no pyvenv.cfg: not a home-level venv"
    );
    assert!(!envs.contains(&deeper), "one level deep only");
}

// T37: a symlink that resolves outside the scanned root is not followed.
#[test]
fn a_symlink_escaping_its_root_is_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let outside = fake_env(&dir.path().join("outside/env"), &[("vllm", "0.29.0")]);
    script(&outside.join("bin/vllm"), "echo 0.29.0");
    std::fs::create_dir_all(home.join("venvs")).unwrap();
    std::os::unix::fs::symlink(&outside, home.join("venvs/escape")).unwrap();
    let inside = fake_env(&home.join("venvs/real"), &[("vllm", "0.29.0")]);
    script(&inside.join("bin/vllm"), "echo 0.29.0");
    std::os::unix::fs::symlink(&inside, home.join("venvs/alias")).unwrap();
    let found = detect(&empty_roots(&home), &ScanBounds::default());
    assert!(
        found
            .iter()
            .all(|c| !c.env.starts_with(dir.path().join("outside"))),
        "{found:?}"
    );
    assert_eq!(
        found.iter().filter(|c| c.engine == Engine::Vllm).count(),
        1,
        "alias deduplicated: {found:?}"
    );
}

// T41 T07: a TensorFold venv, its directory or its bin/tensorfold, resolves
// to <env>/bin/tensorfold from dist-info; 0.6.0 is verified.
#[test]
fn a_tensorfold_environment_resolves_to_its_entry_point() {
    let dir = tempfile::tempdir().unwrap();
    let env = fake_env(&dir.path().join("tf"), &[("tensorfold", "0.6.0")]);
    script(&env.join("bin/tensorfold"), "echo tensorfold 0.6.0");
    for named in [env.clone(), env.join("bin/tensorfold")] {
        let resolved = resolve(&named).unwrap();
        assert_eq!(resolved.engine, Engine::Tensorfold);
        assert_eq!(resolved.version, "0.6.0");
        assert_eq!(resolved.executable, env.join("bin/tensorfold"));
        assert!(!resolved.custom());
    }
    let resolved = resolve(&env).unwrap();
    assert_eq!(
        check_version(&resolved, Duration::from_secs(10)).unwrap(),
        "0.6.0"
    );
}

// T41 T07 (Review Focus 4): two engines in one venv named by its directory
// are refused, naming each entry point.
#[test]
fn an_environment_with_two_engines_names_each_entry_point() {
    let dir = tempfile::tempdir().unwrap();
    let env = fake_env(
        &dir.path().join("both"),
        &[("vllm", "0.29.0"), ("tensorfold", "0.6.0")],
    );
    script(&env.join("bin/vllm"), "echo 0.29.0");
    script(&env.join("bin/tensorfold"), "echo tensorfold 0.6.0");
    let error = resolve(&env).unwrap_err();
    assert_eq!(error.code(), "engine_unsupported");
    let text = error.to_string();
    for entry in ["bin/vllm", "bin/tensorfold"] {
        assert!(text.contains(entry), "{text}");
    }
    assert_eq!(
        resolve(&env.join("bin/tensorfold")).unwrap().engine,
        Engine::Tensorfold
    );
}

// T41 T07 T37: detection lists a TensorFold venv from metadata alone.
#[test]
fn detection_finds_a_tensorfold_environment_and_runs_nothing() {
    let home = tempfile::tempdir().unwrap();
    let env = fake_env(
        &home.path().join("tensorfold-0.6.0-venv"),
        &[("tensorfold", "0.6.0")],
    );
    let marker = home.path().join("ran");
    script(
        &env.join("bin/tensorfold"),
        &format!("touch {}", marker.display()),
    );
    let found = detect(&empty_roots(home.path()), &ScanBounds::default());
    assert!(
        found
            .iter()
            .any(|c| c.engine == Engine::Tensorfold && c.version == "0.6.0"),
        "{found:?}"
    );
    assert!(!marker.exists(), "detection executes nothing");
}

// T37: the scan is bounded in environments and depth.
#[test]
fn the_scan_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let wide = dir.path().join("wide");
    for i in 0..40 {
        let env = fake_env(&wide.join(format!("e{i}")), &[("vllm", "0.29.0")]);
        script(&env.join("bin/vllm"), "echo 0.29.0");
    }
    let deep = fake_env(&dir.path().join("deep/a/b/c/d/env"), &[("vllm", "0.29.0")]);
    script(&deep.join("bin/vllm"), "echo 0.29.0");
    let mut roots = empty_roots(&dir.path().join("nohome"));
    roots.extra = vec![wide.clone(), dir.path().join("deep")];
    let bounds = ScanBounds {
        max_envs: 10,
        max_depth: 3,
        max_dir_entries: 4096,
    };
    let found = detect(&roots, &bounds);
    assert!(found.len() <= 10, "{}", found.len());
    assert!(found.iter().all(|c| c.env != deep), "depth bound exceeded");
}
