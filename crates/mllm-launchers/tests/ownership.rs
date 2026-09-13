use std::process::Command;

#[test]
fn controller_ownership_lasts_until_drop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("controller.lock");
    let first = mllm_launchers::ControllerLock::acquire(&path).unwrap();
    assert!(mllm_launchers::ControllerLock::acquire(&path).is_err());
    drop(first);
    assert!(mllm_launchers::ControllerLock::acquire(&path).is_ok());
}

#[test]
fn controller_lock_is_not_inherited_by_exec_child() {
    let dir = tempfile::tempdir().unwrap();
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
}
