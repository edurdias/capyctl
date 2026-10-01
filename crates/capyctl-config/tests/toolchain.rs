//! ADR 0023 §2: TensorFold's build toolchain, looked up on the closed launch
//! PATH. CPU tests only; they are not qualification.
use capyctl_config::toolchain::check;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
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
