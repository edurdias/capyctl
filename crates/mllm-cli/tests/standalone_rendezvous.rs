//! SPEC §8.2 / T21 (owner decision 2026-09-25): the standalone role keeps SGLang
//! file rendezvous directories in a private root under its state directory, as
//! a host does, so a launch never falls back to /tmp. The root is created 0700
//! at start (an unsafe one is refused), and directories no retained launch owns
//! are swept then, never through a symlink and never outside the root.
//! CPU-only, on the testkit's Fake installation: not qualification of a native
//! recipe (SPEC §18).

mod support;

use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

use mllm_config::effective::ModelSource;
use mllm_controller::LifecyclePort as _;
use support::{engine_ports, safe_state_dir, try_boot_on};

async fn boot(state: &std::path::Path, ports: (u16, u16)) -> mllm_cli::roles::App {
    match try_boot_on(state, ports).await {
        Ok(app) => app,
        Err(error) => panic!("standalone boots: {error}"),
    }
}

fn private(path: &std::path::Path) {
    std::fs::DirBuilder::new().mode(0o700).create(path).unwrap();
    std::fs::write(path.join("store"), b"rendezvous").unwrap();
}

// T21 T33 T37
#[tokio::test]
async fn the_root_is_private_and_a_restart_sweeps_only_unrecorded_directories() {
    let dir = safe_state_dir();
    let root = dir.path().join("rendezvous");
    let ports = engine_ports();
    let app = boot(dir.path(), ports).await;
    let meta = std::fs::symlink_metadata(&root).unwrap();
    assert!(meta.is_dir());
    assert_eq!(meta.mode() & 0o7777, 0o700);
    assert_eq!(meta.uid(), unsafe { libc::geteuid() });

    // One Ready launch, left running across the restart (SPEC §4.3).
    let id = app
        .deploy(
            "rdzv-m",
            ModelSource::Local {
                path: "/models/rdzv-m".into(),
            },
        )
        .unwrap();
    let op = app
        .controller
        .request_transition(&id, mllm_domain::LifecycleAction::Start)
        .await
        .unwrap();
    assert_eq!(
        app.controller.wait_terminal(&op).await.unwrap(),
        mllm_domain::LifecycleState::Ready
    );
    let _ = app.shutdown().await;
    let retained = mllm_store::Store::open(&dir.path().join("server/srv.sqlite3"))
        .unwrap()
        .retained_incarnations()
        .unwrap();
    let recorded = retained
        .iter()
        .next()
        .expect("the Ready launch is retained");

    private(&root.join(recorded));
    private(&root.join("01K00000000000000000000009"));
    let outside = dir.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("data"), b"kept").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("01K0000000000000000000000A")).unwrap();

    let app = boot(dir.path(), ports).await;
    assert!(
        root.join(recorded).join("store").exists(),
        "a retained launch keeps its directory"
    );
    assert!(
        !root.join("01K00000000000000000000009").exists(),
        "a directory no retained launch owns is swept"
    );
    assert!(root
        .join("01K0000000000000000000000A")
        .symlink_metadata()
        .is_ok());
    assert!(outside.join("data").exists(), "a symlink is never followed");
    let _ = app.shutdown().await;
}

// T03 T37
#[tokio::test]
async fn an_unsafe_root_is_refused_without_changing_it() {
    let dir = safe_state_dir();
    let ports = engine_ports();
    let app = boot(dir.path(), ports).await;
    let _ = app.shutdown().await;
    let root = dir.path().join("rendezvous");
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    match try_boot_on(dir.path(), ports).await {
        Ok(_) => panic!("an unsafe rendezvous root must refuse the start"),
        Err(error) => assert!(error.to_string().contains("0700"), "{error}"),
    }
    let mode = std::fs::symlink_metadata(&root).unwrap().mode() & 0o7777;
    assert_eq!(mode, 0o755, "no permissions were changed");

    // A symlink in its place is refused too, and its target is untouched.
    std::fs::remove_dir(&root).unwrap();
    let elsewhere = dir.path().join("elsewhere");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&elsewhere)
        .unwrap();
    std::fs::write(elsewhere.join("data"), b"kept").unwrap();
    std::os::unix::fs::symlink(&elsewhere, &root).unwrap();
    assert!(try_boot_on(dir.path(), ports).await.is_err());
    assert!(elsewhere.join("data").exists());
}
