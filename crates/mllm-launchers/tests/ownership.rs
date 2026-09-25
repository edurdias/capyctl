use std::process::Command;

fn protected_directory() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    directory
}

#[test]
fn controller_ownership_lasts_until_drop() {
    let dir = protected_directory();
    let path = dir.path().join("controller.lock");
    let first = mllm_launchers::ControllerLock::acquire(&path).unwrap();
    assert!(mllm_launchers::ControllerLock::acquire(&path).is_err());
    drop(first);
    assert!(mllm_launchers::ControllerLock::acquire(&path).is_ok());
}

#[test]
fn controller_lock_is_not_inherited_by_exec_child() {
    let dir = protected_directory();
    let path = dir.path().join("controller.lock");
    let _lock = mllm_launchers::ControllerLock::acquire(&path).unwrap();
    let status = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("child_cannot_acquire_parent_controller_lock")
        .arg("--nocapture")
        .env("MLLM_PARENT_LOCK_PATH", &path)
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn child_cannot_acquire_parent_controller_lock() {
    let Ok(path) = std::env::var("MLLM_PARENT_LOCK_PATH") else {
        return;
    };
    assert!(mllm_launchers::ControllerLock::acquire(std::path::Path::new(&path)).is_err());
    for entry in std::fs::read_dir("/proc/self/fd").unwrap() {
        if let Ok(target) = std::fs::read_link(entry.unwrap().path()) {
            assert_ne!(
                target,
                std::path::Path::new(&path),
                "lock descriptor survived exec"
            );
        }
    }
}

#[test]
fn controller_lock_rejects_symlinks_hardlinks_and_nonregular_files() {
    use std::os::unix::fs::symlink;
    let dir = protected_directory();
    let original = dir.path().join("original");
    std::fs::write(&original, b"retained").unwrap();
    let link = dir.path().join("linked");
    symlink(&original, &link).unwrap();
    assert!(mllm_launchers::ControllerLock::acquire(&link).is_err());
    let hard = dir.path().join("hard");
    std::fs::hard_link(&original, &hard).unwrap();
    assert!(mllm_launchers::ControllerLock::acquire(&hard).is_err());
    assert!(mllm_launchers::ControllerLock::acquire(dir.path()).is_err());
    let fifo = dir.path().join("fifo");
    nix::unistd::mkfifo(
        &fifo,
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .unwrap();
    assert!(mllm_launchers::ControllerLock::acquire(&fifo).is_err());
    assert_eq!(std::fs::read(&original).unwrap(), b"retained");
}

#[test]
fn controller_lock_requires_protected_canonical_directory_and_file() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let dir = protected_directory();
    let parent = dir.path().join("service");
    std::fs::create_dir(&parent).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = parent.join("controller.lock");
    for mode in [0o720, 0o702] {
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(mode)).unwrap();
        assert!(mllm_launchers::ControllerLock::acquire(&path).is_err());
        assert!(!path.exists());
    }
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
    drop(mllm_launchers::ControllerLock::acquire(&path).unwrap());
    for mode in [0o620, 0o602] {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        assert!(mllm_launchers::ControllerLock::acquire(&path).is_err());
    }
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let alias = dir.path().join("alias");
    symlink(&parent, &alias).unwrap();
    assert!(mllm_launchers::ControllerLock::acquire(&alias.join("controller.lock")).is_err());
    assert!(
        mllm_launchers::ControllerLock::acquire(&parent.join("../service/controller.lock"))
            .is_err()
    );
    assert!(
        mllm_launchers::ControllerLock::acquire(std::path::Path::new("relative.lock")).is_err()
    );
}
