//! ADR 0018 §1: resolving a named installation and its bounded version
//! check. Fake environments only: CPU evidence, never qualification.
use capyctl_agent::engines::*;
use capyctl_config::engine_policy::Engine;
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
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    capyctl_config::test_support::write_executable(path, format!("#!/bin/sh\n{body}\n"), 0o755)
        .unwrap();
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
        check_version(&resolved, Duration::from_secs(5))
            .unwrap()
            .version,
        "0.29.0"
    );

    std::env::set_var("CAPYCTL_TEST_LEAK", "leaked");
    script(
        &env.join("bin/vllm"),
        "echo \"${CAPYCTL_TEST_LEAK:-0.29.0}\"",
    );
    assert_eq!(
        check_version(&resolved, Duration::from_secs(5))
            .unwrap()
            .version,
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
        system_bins: vec![],
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
    let conda = fake_env(&home.join("miniconda3/envs/sg"), &[("sglang", "0.5.22")]);
    script(&conda.join("bin/python3"), &run_marker);
    let listed = fake_env(&dir.path().join("elsewhere/env"), &[("vllm", "0.30.1")]);
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
    assert!(custom.contains(&"0.5.22".to_string()) && custom.contains(&"0.30.1".to_string()));
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
        check_version(&resolved, Duration::from_secs(10))
            .unwrap()
            .version,
        "0.6.0"
    );
}

// T41 T07 (ADR 0023 §2): two engines in one venv named by its directory
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

/// A llama.cpp build directory: `llama-server` running `body`, and the shared
/// libraries a default (shared) build leaves beside it.
fn llamacpp_build(dir: &Path, body: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    script(&dir.join("llama-server"), body);
    std::fs::write(dir.join("libllama.so.0.6.0"), "elf").unwrap();
    std::os::unix::fs::symlink("libllama.so.0.6.0", dir.join("libllama.so.0")).unwrap();
    std::fs::write(dir.join("libggml-base.so.0.26.0"), "elf").unwrap();
    dir.join("llama-server")
}

/// What `llama-server --version` writes to standard error at the v0.6.0 tag.
const LLAMACPP_VERSION: &str = "echo 'version: 0.6.0 (build 1, commit d812350)' >&2; \
     echo 'built with GNU 15.2.0 for Linux x86_64' >&2";

// T42 T07 (ADR 0029 §2): detection finds `llama-server` in llama.cpp's build
// directories, on PATH and under --path, reads its version from the
// `libllama.so.X.Y.Z` name beside it, and runs nothing.
#[test]
fn detection_lists_a_llama_server_by_its_library_and_runs_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let marker = dir.path().join("ran");
    let run = format!("touch {}", marker.display());
    let built = llamacpp_build(&home.join("llama.cpp/build-cuda/bin"), &run);
    let on_path = llamacpp_build(&dir.path().join("tools/bin"), &run);
    let named = llamacpp_build(&dir.path().join("src/llama.cpp/build/bin"), &run);
    // No library beside it: the version is unknown until `engine add`.
    let bare = dir.path().join("bare/bin");
    std::fs::create_dir_all(&bare).unwrap();
    script(&bare.join("llama-server"), &run);
    let mut roots = empty_roots(&home);
    roots.path_dirs = vec![dir.path().join("tools/bin"), bare.clone()];
    roots.extra = vec![dir.path().join("src")];
    let found = detect(&roots, &ScanBounds::default());
    for (entry, source, version) in [
        (&built, "home", "0.6.0"),
        (&on_path, "PATH", "0.6.0"),
        (&named, "path", "0.6.0"),
        (&bare.join("llama-server"), "PATH", "unknown"),
    ] {
        let candidate = found
            .iter()
            .find(|c| &c.entry == entry)
            .unwrap_or_else(|| panic!("{} missing from {found:?}", entry.display()));
        assert_eq!(candidate.engine, Engine::Llamacpp);
        assert_eq!(candidate.source, source);
        assert_eq!(candidate.version, version);
        assert_eq!(&candidate.env, entry.parent().unwrap());
        // ADR 0029 §3: no llama.cpp build is verified before its live rows.
        assert!(candidate.custom);
    }
    assert!(!marker.exists(), "detection executes nothing");
}

// T42 T07 T37 (ADR 0029 §2): a `llama-server` link that leaves the scanned
// root is not followed; one inside it is listed once, as its target.
#[test]
fn a_llama_server_link_out_of_its_root_is_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    let outside = llamacpp_build(&dir.path().join("outside/bin"), "exit 0");
    let root = dir.path().join("root");
    std::fs::create_dir_all(root.join("escape")).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("escape/llama-server")).unwrap();
    let inside = llamacpp_build(&root.join("real"), "exit 0");
    std::fs::create_dir_all(root.join("alias")).unwrap();
    std::os::unix::fs::symlink(&inside, root.join("alias/llama-server")).unwrap();
    let mut roots = empty_roots(&dir.path().join("nohome"));
    roots.extra = vec![root.clone()];
    let found = detect(&roots, &ScanBounds::default());
    let llama: Vec<_> = found
        .iter()
        .filter(|c| c.engine == Engine::Llamacpp)
        .collect();
    assert_eq!(llama.len(), 1, "{found:?}");
    assert_eq!(llama[0].entry, inside.canonicalize().unwrap());
}

// T42 (ADR 0029 §2): the binary or its directory resolves; a link is named
// by what it points to; nothing runs until the version check.
#[test]
fn a_llama_server_binary_or_its_directory_resolves() {
    let dir = tempfile::tempdir().unwrap();
    let binary = llamacpp_build(&dir.path().join("build/bin"), LLAMACPP_VERSION);
    for named in [binary.clone(), dir.path().join("build/bin")] {
        let resolved = resolve(&named).unwrap();
        assert_eq!(resolved.engine, Engine::Llamacpp);
        assert_eq!(resolved.executable, binary);
        assert_eq!(resolved.env, dir.path().join("build/bin"));
        assert_eq!(resolved.version, "0.6.0");
    }
    let link = dir.path().join("llama-server");
    std::os::unix::fs::symlink(&binary, &link).unwrap();
    let error = resolve(&link).unwrap_err();
    assert_eq!(error.code(), "engine_unsupported");
    assert!(
        error.to_string().contains(&binary.display().to_string()),
        "{error}"
    );
    assert_eq!(
        resolve(&dir.path().join("none/llama-server"))
            .unwrap_err()
            .code(),
        "engine_not_found"
    );
}

// T42 T37 (ADR 0029 §2): the version is read from standard error; the build
// number is not kept; the fingerprint is `<version>+<commit>`; a version on
// standard output alone does not parse.
#[test]
fn the_llama_server_version_is_read_from_standard_error() {
    let dir = tempfile::tempdir().unwrap();
    let binary = llamacpp_build(&dir.path().join("bin"), LLAMACPP_VERSION);
    let resolved = resolve(&binary).unwrap();
    let reported = check_version(&resolved, Duration::from_secs(5)).unwrap();
    assert_eq!(reported.version, "0.6.0");
    assert_eq!(reported.build_fingerprint, "0.6.0+d812350");
    script(
        &binary,
        "echo 'version: 0.6.0-dev (build 9137, commit d812350)' >&2",
    );
    let reported = check_version(&resolved, Duration::from_secs(5)).unwrap();
    assert_eq!(reported.version, "0.6.0-dev");
    assert_eq!(reported.build_fingerprint, "0.6.0-dev+d812350");
    script(&binary, "echo 'version: 0.6.0 (build 1, commit d812350)'");
    let error = check_version(&resolved, Duration::from_secs(5)).unwrap_err();
    assert_eq!(error, VersionCheckError::Unparsable);
    assert_eq!(error.code(), "engine_unsupported");
    std::env::set_var("CAPYCTL_TEST_LLAMA_LEAK", "9.9.9");
    script(
        &binary,
        "echo \"version: ${CAPYCTL_TEST_LLAMA_LEAK:-0.6.0} (build 1, commit d812350)\" >&2",
    );
    assert_eq!(
        check_version(&resolved, Duration::from_secs(5))
            .unwrap()
            .version,
        "0.6.0",
        "the check sees no inherited environment"
    );
    script(&binary, "yes x >&2");
    assert_eq!(
        check_version(&resolved, Duration::from_secs(5)).unwrap_err(),
        VersionCheckError::Output
    );
}

// ---- ADR 0039 §2: TensorFold's native release ----

/// The cubin a fake release ships (`share/tensorfold/cuda/sm121/`).
const CUBIN: &str = "share/tensorfold/cuda/sm121/kernels.cubin";

/// The capabilities document `tensorfold-native capabilities --json` writes
/// (`server/cli.zig:250-288` at v1.0.0): every flag CapyCTL renders, the
/// memory cap among `env`, one family.
fn native_capabilities_doc(version: &str) -> serde_json::Value {
    let mut flags = serde_json::Map::new();
    for (flag, _) in capyctl_config::tensorfold_native::REQUIRED_FLAGS {
        flags.insert((*flag).into(), serde_json::json!({}));
    }
    flags.insert(
        "--backend".into(),
        serde_json::json!({"values": ["auto", "cuda"]}),
    );
    flags.insert("--compact-at".into(), serde_json::json!({}));
    serde_json::json!({
        "schema": 1, "engine": "zig", "version": version, "chip": "nvidia-sm121",
        "backends": ["cuda"], "families": {"nemotron_h": ["mlx-q4g64"]},
        "serve": {"flags": flags},
        "env": ["TENSORFOLD_CUDA_MEMORY_LIMIT_GB", "TENSORFOLD_NO_UPDATE_CHECK"],
    })
}

/// `bin/tensorfold-native` answering `--version` and `capabilities --json`.
fn native_binary(root: &Path, version: &str, capabilities: &serde_json::Value) -> PathBuf {
    let binary = root.join("bin/tensorfold-native");
    script(
        &binary,
        &format!(
            "case \"$1\" in --version) echo 'tensorfold-native {version}';; \
             capabilities) printf '%s\\n' '{capabilities}';; *) exit 2;; esac"
        ),
    );
    binary
}

/// `SHA256SUMS` over `files` (relative to `root`), as `sha256sum` writes it.
fn write_sha256sums(root: &Path, files: &[&str]) {
    use sha2::Digest as _;
    let text: String = files
        .iter()
        .map(|file| {
            let digest = sha2::Sha256::digest(std::fs::read(root.join(file)).unwrap());
            format!("{}  {file}\n", hex::encode(digest))
        })
        .collect();
    std::fs::write(root.join("SHA256SUMS"), text).unwrap();
}

/// A native release root as the Linux archive unpacks it:
/// `bin/tensorfold-native`, `VERSION`, the CUDA kernel set
/// (`share/tensorfold/cuda/sm121/aot.json` and a cubin), a notice and
/// `SHA256SUMS` over them.
fn native_release(root: &Path, version: &str) -> PathBuf {
    let binary = native_binary(root, version, &native_capabilities_doc(version));
    std::fs::write(root.join("VERSION"), format!("{version}\n")).unwrap();
    let kernels = root.join("share/tensorfold/cuda/sm121");
    std::fs::create_dir_all(&kernels).unwrap();
    std::fs::write(kernels.join("aot.json"), "{\"arch\": \"sm_121\"}\n").unwrap();
    std::fs::write(root.join(CUBIN), b"\x7fELF cubin v1").unwrap();
    std::fs::write(root.join("NOTICE"), "TensorFold\n").unwrap();
    write_sha256sums(
        root,
        &[
            "bin/tensorfold-native",
            "VERSION",
            "share/tensorfold/cuda/sm121/aot.json",
            CUBIN,
            "NOTICE",
        ],
    );
    binary
}

// T41 T07 T37 (ADR 0039 §2): detection finds `tensorfold-native` in
// `~/*/bin`, a PATH entry, `/opt/*/bin`, `/usr/local/bin` and under --path,
// reads its version from the root's `VERSION` file (else `unknown`), shows
// the native line, and executes nothing.
#[test]
fn detection_lists_a_native_release_by_its_version_file_and_runs_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let marker = dir.path().join("ran");
    let run = format!("touch {}", marker.display());
    let release = |root: &Path, version: Option<&str>| {
        script(&root.join("bin/tensorfold-native"), &run);
        if let Some(version) = version {
            std::fs::write(root.join("VERSION"), format!("{version}\n")).unwrap();
        }
        root.to_path_buf()
    };
    let in_home = release(&home.join("tensorfold-1.0.0"), Some("1.0.0"));
    let on_path = release(&dir.path().join("tools/tf-1.0.3"), Some("1.0.3"));
    let in_opt = release(&dir.path().join("opt/tensorfold-1.1.5"), Some("1.1.5"));
    let system = release(&dir.path().join("usr/local"), None);
    let named = release(
        &dir.path().join("src/releases/tensorfold-1.2.0"),
        Some("1.2.0"),
    );
    // A VERSION that is not one MAJOR.MINOR.PATCH line reads as unknown.
    let odd = release(&home.join("tensorfold-odd"), None);
    std::fs::write(odd.join("VERSION"), "1.0\n").unwrap();
    let mut roots = empty_roots(&home);
    roots.path_dirs = vec![on_path.join("bin")];
    roots.opt = Some(dir.path().join("opt"));
    roots.system_bins = vec![system.join("bin")];
    roots.extra = vec![dir.path().join("src")];
    let found = detect(&roots, &ScanBounds::default());
    for (root, source, version) in [
        (&in_home, "home", "1.0.0"),
        (&on_path, "PATH", "1.0.3"),
        (&in_opt, "opt", "1.1.5"),
        (&system, "system", "unknown"),
        (&named, "path", "1.2.0"),
        (&odd, "home", "unknown"),
    ] {
        let candidate = found
            .iter()
            .find(|c| &c.env == root)
            .unwrap_or_else(|| panic!("{} missing from {found:?}", root.display()));
        assert_eq!(candidate.engine, Engine::Tensorfold);
        assert_eq!(
            candidate.line,
            capyctl_config::tensorfold_native::TensorfoldLine::Native
        );
        assert_eq!(candidate.entry, root.join("bin/tensorfold-native"));
        assert_eq!(candidate.source, source);
        assert_eq!(candidate.version, version);
        // ADR 0039 §3: no native version is certified before its live rows.
        assert!(candidate.custom);
    }
    assert!(!marker.exists(), "detection executes nothing");
}

// T41 T37 (ADR 0039 §2): a `tensorfold-native` link that leaves the scanned
// root is not followed.
#[test]
fn a_native_binary_link_out_of_its_root_is_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    let outside = native_release(&dir.path().join("outside"), "1.0.0");
    let home = dir.path().join("home");
    std::fs::create_dir_all(home.join("tf/bin")).unwrap();
    std::os::unix::fs::symlink(&outside, home.join("tf/bin/tensorfold-native")).unwrap();
    let found = detect(&empty_roots(&home), &ScanBounds::default());
    assert!(
        found.iter().all(|c| c.engine != Engine::Tensorfold),
        "{found:?}"
    );
}

// T41 T07 (ADR 0039 §2): the binary, its bin directory or the release root
// resolve to the native line, rooted at the directory above bin, with the
// VERSION file's version; a link and a binary outside a bin directory are
// refused; nothing runs.
#[test]
fn a_native_release_resolves_from_its_binary_its_bin_or_its_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("tensorfold-1.0.0");
    let binary = native_release(&root, "1.0.0");
    for named in [binary.clone(), root.join("bin"), root.clone()] {
        let resolved = resolve(&named).unwrap();
        assert_eq!(resolved.engine, Engine::Tensorfold);
        assert_eq!(
            resolved.line,
            capyctl_config::tensorfold_native::TensorfoldLine::Native
        );
        assert_eq!(resolved.executable, binary);
        assert_eq!(resolved.env, root);
        assert_eq!(resolved.version, "1.0.0");
    }
    let link = dir.path().join("tensorfold-native");
    std::os::unix::fs::symlink(&binary, &link).unwrap();
    let error = resolve(&link).unwrap_err();
    assert_eq!(error.code(), "engine_unsupported");
    assert!(
        error.to_string().contains(&binary.display().to_string()),
        "{error}"
    );
    let loose = dir.path().join("loose");
    script(&loose.join("tensorfold-native"), "exit 0");
    let error = resolve(&loose.join("tensorfold-native")).unwrap_err();
    assert_eq!(error.code(), "engine_unsupported");
    assert!(error.to_string().contains("bin directory"), "{error}");
    assert_eq!(
        resolve(&dir.path().join("none/bin/tensorfold-native"))
            .unwrap_err()
            .code(),
        "engine_not_found"
    );
    // Without a VERSION file the version is unknown until the check reads it.
    std::fs::remove_file(root.join("VERSION")).unwrap();
    assert_eq!(resolve(&root).unwrap().version, "unknown");
}

// T41 T17 (ADR 0039 §2, §16): a `bin/tensorfold` outside a Python
// environment, or beside `bin/tensorfold-native` in a release root, is
// refused `engine_unsupported`, naming the native binary.
#[test]
fn a_stray_bin_tensorfold_outside_a_python_environment_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let stray = dir.path().join("stray");
    script(&stray.join("bin/tensorfold"), "echo tensorfold 0.6.0");
    let error = resolve(&stray.join("bin/tensorfold")).unwrap_err();
    assert_eq!(error.code(), "engine_unsupported");
    assert!(error.to_string().contains("tensorfold-native"), "{error}");
    let root = dir.path().join("tensorfold-1.0.0");
    native_release(&root, "1.0.0");
    script(&root.join("bin/tensorfold"), "echo tensorfold 0.6.0");
    let error = resolve(&root.join("bin/tensorfold")).unwrap_err();
    assert_eq!(error.code(), "engine_unsupported");
    assert!(
        error
            .to_string()
            .contains(&root.join("bin/tensorfold-native").display().to_string()),
        "{error}"
    );
    // The 0.6 environment still resolves as before.
    let env = fake_env(&dir.path().join("tf"), &[("tensorfold", "0.6.0")]);
    script(&env.join("bin/tensorfold"), "echo tensorfold 0.6.0");
    let resolved = resolve(&env.join("bin/tensorfold")).unwrap();
    assert_eq!(
        resolved.line,
        capyctl_config::tensorfold_native::TensorfoldLine::Python
    );
}

// T41 T17 T37 (ADR 0039 §2, §16): the native version is read from standard
// output, exactly `tensorfold-native <MAJOR.MINOR.PATCH>`; a line that cannot
// be read, and a VERSION file that disagrees, are `engine_version_failed`
// (18). The checked version is the build fingerprint.
#[test]
fn the_native_version_is_read_from_standard_output() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("tensorfold-1.0.0");
    let binary = native_release(&root, "1.0.0");
    let resolved = resolve(&binary).unwrap();
    let reported = check_version(&resolved, Duration::from_secs(5)).unwrap();
    assert_eq!(reported.version, "1.0.0");
    assert_eq!(reported.build_fingerprint, "1.0.0");
    for body in [
        // On standard error only: standard output is what is read.
        "echo 'tensorfold-native 1.0.0' >&2",
        "echo 'tensorfold 1.0.0'",
        "echo 'tensorfold-native one'",
        "echo 'tensorfold-native 1.0.0'; echo extra",
    ] {
        script(&binary, body);
        let error = check_version(&resolved, Duration::from_secs(5)).unwrap_err();
        assert_eq!(error, VersionCheckError::NativeUnreadable, "{body}");
        assert_eq!(error.code(), "engine_version_failed");
    }
    script(&binary, "echo 'tensorfold-native 1.0.1'");
    let error = check_version(&resolved, Duration::from_secs(5)).unwrap_err();
    assert_eq!(error.code(), "engine_version_failed");
    let text = error.to_string();
    assert!(text.contains("1.0.1") && text.contains("1.0.0"), "{text}");
    std::fs::write(root.join("VERSION"), "1.0\n").unwrap();
    let error = check_version(&resolved, Duration::from_secs(5)).unwrap_err();
    assert_eq!(
        error,
        VersionCheckError::VersionFile {
            reported: "1.0.1".into(),
            stated: None
        }
    );
    // No VERSION file: the binary's own line is the version.
    std::fs::remove_file(root.join("VERSION")).unwrap();
    assert_eq!(
        check_version(&resolved, Duration::from_secs(5))
            .unwrap()
            .version,
        "1.0.1"
    );
    std::env::set_var("CAPYCTL_TEST_NATIVE_LEAK", "9.9.9");
    script(
        &binary,
        "echo \"tensorfold-native ${CAPYCTL_TEST_NATIVE_LEAK:-1.0.1}\"",
    );
    assert_eq!(
        check_version(&resolved, Duration::from_secs(5))
            .unwrap()
            .version,
        "1.0.1",
        "the check sees no inherited environment"
    );
}

// T41 T17 (ADR 0039 §2 step 2): every file SHA256SUMS lists is checked; a
// changed or missing one refuses the installation `engine_unsupported`,
// naming it; a root without SHA256SUMS registers unverified.
#[test]
fn a_changed_or_missing_file_under_sha256sums_refuses_the_release() {
    use capyctl_agent::installation::verify_checksums;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("tensorfold-1.0.0");
    native_release(&root, "1.0.0");
    assert_eq!(verify_checksums(&root), Ok(true));
    std::fs::write(root.join(CUBIN), b"\x7fELF cubin v2").unwrap();
    let error = verify_checksums(&root).unwrap_err();
    assert_eq!(error.code(), "engine_unsupported");
    assert!(error.to_string().contains(CUBIN), "{error}");
    assert!(
        error.to_string().contains("changed after it was unpacked"),
        "{error}"
    );
    std::fs::write(root.join(CUBIN), b"\x7fELF cubin v1").unwrap();
    assert_eq!(verify_checksums(&root), Ok(true));
    std::fs::remove_file(root.join("NOTICE")).unwrap();
    let error = verify_checksums(&root).unwrap_err();
    assert_eq!(error.code(), "engine_unsupported");
    assert!(
        error
            .to_string()
            .starts_with("NOTICE, listed in SHA256SUMS, is missing"),
        "{error}"
    );
    // The binary-mode form (`<hex> *<path>`) is read too.
    std::fs::write(root.join("NOTICE"), "TensorFold\n").unwrap();
    let text = std::fs::read_to_string(root.join("SHA256SUMS")).unwrap();
    std::fs::write(root.join("SHA256SUMS"), text.replace("  ", " *")).unwrap();
    assert_eq!(verify_checksums(&root), Ok(true));
    // A path leaving the root, by `..` or by a link, is refused.
    let outside = dir.path().join("outside.txt");
    std::fs::write(&outside, "x").unwrap();
    std::fs::write(
        root.join("SHA256SUMS"),
        format!("{}  ../outside.txt\n", "0".repeat(64)),
    )
    .unwrap();
    assert!(verify_checksums(&root)
        .unwrap_err()
        .to_string()
        .contains("not a relative path inside the release"));
    std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
    write_sha256sums(&root, &["escape"]);
    assert!(verify_checksums(&root)
        .unwrap_err()
        .to_string()
        .contains("outside the release directory"));
    std::fs::write(root.join("SHA256SUMS"), "not a checksum line\n").unwrap();
    assert_eq!(
        verify_checksums(&root).unwrap_err().code(),
        "engine_unsupported"
    );
    std::fs::remove_file(root.join("SHA256SUMS")).unwrap();
    assert_eq!(verify_checksums(&root), Ok(false));
}

// T41 T17 (ADR 0039 §2 step 3): the capabilities document must carry
// schema 1, engine zig, the checked version, the cuda backend, every flag
// CapyCTL renders (with the value it renders) and the memory cap variable;
// anything missing is `engine_unsupported`, naming it. Extra keys are fine.
#[test]
fn the_capabilities_document_must_list_what_capyctl_renders() {
    use capyctl_agent::installation::parse_native_capabilities;
    let good = native_capabilities_doc("1.1.5");
    let mut extra = good.clone();
    extra["optional_libraries"] = serde_json::json!({"nccl": false});
    let families = parse_native_capabilities(extra.to_string().as_bytes(), "1.1.5").unwrap();
    assert_eq!(
        families,
        [("nemotron_h".to_owned(), vec!["mlx-q4g64".to_owned()])].into()
    );
    let refused = |mutate: &dyn Fn(&mut serde_json::Value), named: &str| {
        let mut document = good.clone();
        mutate(&mut document);
        let error =
            parse_native_capabilities(document.to_string().as_bytes(), "1.1.5").unwrap_err();
        assert_eq!(error.code(), "engine_unsupported");
        assert!(error.to_string().contains(named), "{named}: {error}");
    };
    refused(
        &|d| {
            d["serve"]["flags"]
                .as_object_mut()
                .unwrap()
                .remove("--no-drafts");
        },
        "--no-drafts",
    );
    refused(
        &|d| d["serve"]["flags"]["--backend"] = serde_json::json!({"values": ["auto", "mlx"]}),
        "--backend cuda",
    );
    refused(
        &|d| d["backends"] = serde_json::json!(["metal"]),
        "cuda backend",
    );
    refused(
        &|d| d["env"] = serde_json::json!(["TENSORFOLD_NO_UPDATE_CHECK"]),
        "TENSORFOLD_CUDA_MEMORY_LIMIT_GB",
    );
    refused(&|d| d["version"] = serde_json::json!("1.1.4"), "1.1.4");
    refused(&|d| d["schema"] = serde_json::json!(2), "schema");
    refused(&|d| d["engine"] = serde_json::json!("python"), "engine");
    assert!(parse_native_capabilities(b"not json", "1.1.5").is_err());
}

// T41 T17 T37 (ADR 0039 §2): `capabilities --json` runs bounded, its
// standard output read; with SHA256SUMS the record says checked, without it
// unchecked.
#[test]
fn a_native_release_is_checked_into_its_registration_record() {
    use capyctl_agent::installation::check_native_release;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("tensorfold-1.0.0");
    let binary = native_release(&root, "1.0.0");
    let record = check_native_release(&binary, "1.0.0", Duration::from_secs(5)).unwrap();
    assert!(record.checksums_verified);
    assert_eq!(record.families["nemotron_h"], vec!["mlx-q4g64".to_owned()]);
    std::fs::remove_file(root.join("SHA256SUMS")).unwrap();
    let record = check_native_release(&binary, "1.0.0", Duration::from_secs(5)).unwrap();
    assert!(!record.checksums_verified);
    let mut missing = native_capabilities_doc("1.0.0");
    missing["env"] = serde_json::json!([]);
    native_binary(&root, "1.0.0", &missing);
    let error = check_native_release(&binary, "1.0.0", Duration::from_secs(5)).unwrap_err();
    assert_eq!(error.code(), "engine_unsupported");
    assert!(
        error
            .to_string()
            .contains("TENSORFOLD_CUDA_MEMORY_LIMIT_GB"),
        "{error}"
    );
    script(&binary, "yes x");
    assert!(
        check_native_release(&binary, "1.0.0", Duration::from_secs(5))
            .unwrap_err()
            .to_string()
            .contains("more than")
    );
}

// T41 T22 (ADR 0039 §2, ADR 0008): the native fingerprint covers every
// regular file under the release root, so a replaced cubin changes it; the
// host measures a published native profile the same way.
#[test]
fn the_native_digest_changes_with_a_cubin() {
    use capyctl_agent::installation::{register_profile, InstallationMeasurer};
    use capyctl_config::tensorfold_native::TensorfoldLine;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("tensorfold-1.0.0");
    let binary = native_release(&root, "1.0.0");
    let measure = || {
        InstallationMeasurer::new()
            .measure_profile(Engine::Tensorfold, TensorfoldLine::Native, &binary, None)
            .unwrap()
    };
    let first = measure();
    assert_eq!(first.version, "1.0.0");
    // bin/tensorfold-native, VERSION, aot.json, the cubin, NOTICE, SHA256SUMS.
    assert_eq!(first.files, 6);
    let profile = serde_json::json!({"engine": "tensorfold", "line": "native",
        "executable": binary, "build_fingerprint": "1.0.0"});
    let mut status = capyctl_protocol::pb::RuntimeProfileStatus::default();
    register_profile(&InstallationMeasurer::new(), &profile, &mut status);
    assert_eq!(status.installation_state, "measured");
    assert_eq!(status.installation_digest, first.digest);
    std::fs::write(root.join(CUBIN), b"\x7fELF cubin v2").unwrap();
    assert_ne!(measure().digest, first.digest);
    // The Python line's measurement finds no package beside this binary.
    assert!(InstallationMeasurer::new()
        .measure(Engine::Tensorfold, &binary)
        .is_err());
}

// T41 (ADR 0039 §3): a native version outside the certified set is custom
// and runs with a note, never refused; 1.0.0 and 1.1.5 count as certified
// once their rows pass (a test-stated table), and until then say which rows
// certify them.
#[test]
fn native_versions_are_custom_until_certified() {
    use capyctl_config::tensorfold_native::{
        is_certified_in, version_note_in, Certification, TensorfoldLine,
    };
    let passed = [
        Certification {
            version: "1.0.0",
            rows: "TF1 to TF10",
            passed: true,
        },
        Certification {
            version: "1.1.5",
            rows: "TF11 to TF14",
            passed: true,
        },
    ];
    for version in ["1.0.0", "1.1.5"] {
        assert!(is_certified_in(&passed, version));
        assert_eq!(version_note_in(&passed, version), None);
    }
    for version in ["1.0.3", "1.2.0"] {
        assert!(!is_certified_in(&passed, version));
        let note = version_note_in(&passed, version).unwrap();
        assert!(note.contains("1.0.0 and 1.1.5"), "{note}");
        let resolved = Resolved {
            engine: Engine::Tensorfold,
            version: version.into(),
            env: "/r".into(),
            executable: "/r/bin/tensorfold-native".into(),
            line: TensorfoldLine::Native,
        };
        assert!(resolved.custom());
    }
    // A native 0.6.6 is not read against the Python line's verified set.
    let native_066 = Resolved {
        engine: Engine::Tensorfold,
        version: "0.6.6".into(),
        env: "/r".into(),
        executable: "/r/bin/tensorfold-native".into(),
        line: TensorfoldLine::Native,
    };
    assert!(native_066.custom());
    assert!(!Resolved {
        line: TensorfoldLine::Python,
        ..native_066
    }
    .custom());
}
