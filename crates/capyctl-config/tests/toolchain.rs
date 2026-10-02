//! ADR 0023 §2: TensorFold's build toolchain, looked up on the closed launch
//! PATH. CPU tests only; they are not qualification.
use capyctl_config::toolchain::check;
use std::path::Path;

fn script(path: &Path, body: &str) {
    capyctl_config::test_support::write_executable(path, format!("#!/bin/sh\n{body}\n"), 0o755)
        .unwrap();
}

// T41 T37: the toolchain is looked up on the closed launch PATH only, in
// order, and nothing is executed.
#[test]
fn the_toolchain_is_found_on_the_closed_path_only() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("env/bin");
    let cuda = dir.path().join("cuda");
    let system = dir.path().join("system");
    for d in [&bin, &cuda.join("bin"), &system] {
        std::fs::create_dir_all(d).unwrap();
    }
    let marker = dir.path().join("ran");
    let touch = format!("touch {}", marker.display());
    let system_path = system.to_string_lossy().into_owned();
    let missing = check(&bin, Some(&cuda), &system_path).unwrap_err();
    assert_eq!(missing.missing, vec!["ninja", "nvcc", "c++ or g++"]);
    assert_eq!(
        missing.searched,
        vec![bin.clone(), cuda.join("bin"), system.clone()]
    );
    let text = missing.to_string();
    assert!(
        text.contains("ninja") && text.contains(&bin.display().to_string()),
        "{text}"
    );
    script(&bin.join("ninja"), &touch);
    script(&cuda.join("bin/nvcc"), &touch);
    script(&system.join("g++"), &touch);
    check(&bin, Some(&cuda), &system_path).unwrap();
    // Without cuda_home its bin is not searched.
    assert_eq!(
        check(&bin, None, &system_path).unwrap_err().missing,
        vec!["nvcc"]
    );
    // A directory or a non-executable file is not a tool.
    std::fs::remove_file(cuda.join("bin/nvcc")).unwrap();
    std::fs::create_dir(cuda.join("bin/nvcc")).unwrap();
    std::fs::write(system.join("nvcc"), "").unwrap();
    assert_eq!(
        check(&bin, Some(&cuda), &system_path).unwrap_err().missing,
        vec!["nvcc"]
    );
    // Found in a later directory of the closed PATH, in order.
    script(&system.join("nvcc"), &touch);
    check(&bin, Some(&cuda), &system_path).unwrap();
    assert!(!marker.exists(), "the check executes nothing");
}

// T41 T03: a pip-only CUDA compiler puts nvcc under the environment's
// site-packages/nvidia/cu<major>/bin; it counts, nothing runs, and a link
// pointing out of the environment is not followed.
#[test]
fn a_pip_nvcc_in_site_packages_satisfies_the_check() {
    let dir = tempfile::tempdir().unwrap();
    let env = dir.path().join("env");
    let bin = env.join("bin");
    let system = dir.path().join("system");
    let nvidia = env.join("lib/python3.12/site-packages/nvidia");
    let pip_bin = nvidia.join("cu13/bin");
    for d in [&bin, &system, &pip_bin] {
        std::fs::create_dir_all(d).unwrap();
    }
    let marker = dir.path().join("ran");
    let touch = format!("touch {}", marker.display());
    script(&bin.join("ninja"), &touch);
    script(&system.join("c++"), &touch);
    let system_path = system.to_string_lossy().into_owned();
    let missing = check(&bin, None, &system_path).unwrap_err();
    assert_eq!(missing.missing, vec!["nvcc"]);
    for text in [missing.for_engine_add(), missing.for_local_engine()] {
        assert!(text.contains("pip install"), "{text}");
    }
    script(&pip_bin.join("nvcc"), &touch);
    check(&bin, None, &system_path).unwrap();
    assert!(!marker.exists(), "the check executes nothing");
    // A link leading out of the environment is not followed.
    std::fs::remove_dir_all(&nvidia).unwrap();
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(outside.join("cu13/bin")).unwrap();
    script(&outside.join("cu13/bin/nvcc"), &touch);
    std::os::unix::fs::symlink(&outside, &nvidia).unwrap();
    assert_eq!(
        check(&bin, None, &system_path).unwrap_err().missing,
        vec!["nvcc"]
    );
}
