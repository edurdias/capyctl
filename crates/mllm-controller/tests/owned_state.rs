use mllm_controller::OwnedCoordinatorState;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::Path;

fn state_dir() -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("mllm-owned-state-")
        .tempdir_in(std::env::var_os("HOME").unwrap())
        .unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn snapshot(dir: &Path) -> (i64, String, i64) {
    let db = rusqlite::Connection::open_with_flags(
        dir.join("srv.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    db.query_row("SELECT epoch, session_id, (SELECT COUNT(*) FROM management_events) FROM coordinator_session", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap()
}

#[test]
fn contention_cannot_start_session_and_restart_advances_once() {
    let dir = state_dir();
    let first = OwnedCoordinatorState::open(dir.path()).unwrap();
    assert_eq!(first.session().epoch(), 1);
    assert_eq!(first.store().deployment_count().unwrap(), 0);
    let before = snapshot(dir.path());
    assert!(OwnedCoordinatorState::open(dir.path()).is_err());
    assert_eq!(snapshot(dir.path()), before);
    drop(first);
    let second = OwnedCoordinatorState::open(dir.path()).unwrap();
    assert_eq!(second.session().epoch(), 2);
    assert_ne!(second.session().id(), before.1);
    assert_eq!(snapshot(dir.path()).2, before.2 + 1);
}

#[test]
fn malformed_database_failure_releases_lifetime_lock() {
    let dir = state_dir();
    let path = dir.path().join("srv.sqlite3");
    fs::write(&path, b"not sqlite").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(OwnedCoordinatorState::open(dir.path()).is_err());
    assert!(mllm_launchers::ControllerLock::acquire(&dir.path().join("controller.lock")).is_ok());
}

#[test]
fn session_failure_rolls_back_and_releases_lock() {
    let dir = state_dir();
    drop(OwnedCoordinatorState::open(dir.path()).unwrap());
    let db = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    db.execute("UPDATE coordinator_session SET epoch=?1", [i64::MAX])
        .unwrap();
    let before = snapshot(dir.path());
    assert!(matches!(
        OwnedCoordinatorState::open(dir.path()),
        Err(mllm_controller::OwnedStateError::Session(_))
    ));
    assert_eq!(snapshot(dir.path()), before);
    let lock =
        mllm_launchers::ControllerLock::acquire(&dir.path().join("controller.lock")).unwrap();
    drop(lock);
    db.execute("UPDATE coordinator_session SET epoch=1", [])
        .unwrap();
    assert_eq!(
        OwnedCoordinatorState::open(dir.path())
            .unwrap()
            .session()
            .epoch(),
        2
    );
}

#[test]
fn losing_contender_preserves_dispatch_and_request_lease() {
    let dir = state_dir();
    let owner = OwnedCoordinatorState::open(dir.path()).unwrap();
    let db = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    db.execute_batch("INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version,dispatch_enabled) VALUES('d','d','managed','ready',1,0,1,1,1);
      INSERT INTO request_leases(id,deployment_id,revision,generation,session_id,disposition) VALUES('lease','d',1,1,'test-session','inflight');").unwrap();
    let state = || {
        db.query_row("SELECT dispatch_enabled, (SELECT disposition FROM request_leases WHERE id='lease') FROM deployments WHERE id='d'", [], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))).unwrap()
    };
    let before = snapshot(dir.path());
    assert!(OwnedCoordinatorState::open(dir.path()).is_err());
    assert_eq!(state(), (1, "inflight".into()));
    assert_eq!(snapshot(dir.path()), before);
    drop(owner);
    let _restart = OwnedCoordinatorState::open(dir.path()).unwrap();
    assert_eq!(state(), (0, "uncertain".into()));
}

#[test]
fn unsafe_database_and_sidecars_are_rejected_without_following_or_chmod() {
    for name in [
        "srv.sqlite3",
        "srv.sqlite3-wal",
        "srv.sqlite3-shm",
        "srv.sqlite3-journal",
    ] {
        for kind in ["symlink", "hardlink", "directory", "writable", "readable"] {
            let dir = state_dir();
            let target = dir.path().join("untouched");
            fs::write(&target, b"must remain unchanged").unwrap();
            fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
            let path = dir.path().join(name);
            match kind {
                "symlink" => symlink(&target, &path).unwrap(),
                "hardlink" => fs::hard_link(&target, &path).unwrap(),
                "directory" => fs::create_dir(&path).unwrap(),
                _ => {
                    fs::write(&path, b"unsafe").unwrap();
                    fs::set_permissions(
                        &path,
                        fs::Permissions::from_mode(if kind == "writable" { 0o620 } else { 0o640 }),
                    )
                    .unwrap();
                }
            }
            let before_mode = fs::symlink_metadata(&path).unwrap().mode();
            assert!(
                matches!(
                    OwnedCoordinatorState::open(dir.path()),
                    Err(mllm_controller::OwnedStateError::Ownership(_))
                ),
                "{name}/{kind} must fail before opening SQLite"
            );
            assert_eq!(fs::read(&target).unwrap(), b"must remain unchanged");
            assert_eq!(fs::metadata(&target).unwrap().mode() & 0o777, 0o600);
            assert_eq!(fs::symlink_metadata(&path).unwrap().mode(), before_mode);
            if name != "srv.sqlite3" {
                assert!(!dir.path().join("srv.sqlite3").exists());
            }
        }
    }
}

#[test]
fn unprotected_or_aliased_state_directory_is_not_modified() {
    let dir = state_dir();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o750)).unwrap();
    assert!(OwnedCoordinatorState::open(dir.path()).is_err());
    assert_eq!(fs::metadata(dir.path()).unwrap().mode() & 0o777, 0o750);
    assert!(!dir.path().join("controller.lock").exists());
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let alias = dir.path().join("alias");
    symlink(dir.path(), &alias).unwrap();
    assert!(OwnedCoordinatorState::open(&alias).is_err());
    assert!(OwnedCoordinatorState::open(&dir.path().join(".")).is_err());
    assert!(OwnedCoordinatorState::open(Path::new("relative-state")).is_err());
}

#[test]
fn child_process_cannot_start_a_second_session() {
    if let Some(path) = std::env::var_os("MLLM_OWNED_STATE_CHILD_DIR") {
        assert!(OwnedCoordinatorState::open(Path::new(&path)).is_err());
        return;
    }
    let dir = state_dir();
    let _owner = OwnedCoordinatorState::open(dir.path()).unwrap();
    let before = snapshot(dir.path());
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_process_cannot_start_a_second_session"])
        .env("MLLM_OWNED_STATE_CHILD_DIR", dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(snapshot(dir.path()), before);
}
