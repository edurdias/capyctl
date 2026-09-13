use std::sync::Mutex;

use mllm_adapters::traits::RenderedCommand;
use mllm_domain::completion::ProcessIdentity;
use mllm_launchers::{AssociationError, DurableSpawn, DurableSpawnOutcome, LaunchAssociation};

struct RecordingAssociation {
    identities: Mutex<Vec<ProcessIdentity>>,
    fail: bool,
}

impl LaunchAssociation for RecordingAssociation {
    fn persist_api_identity(&self, identity: &ProcessIdentity) -> Result<(), AssociationError> {
        self.identities.lock().unwrap().push(identity.clone());
        if self.fail {
            Err(AssociationError::Uncertain("commit outcome unknown".into()))
        } else {
            Ok(())
        }
    }
}

fn marker_command(path: &std::path::Path) -> RenderedCommand {
    RenderedCommand {
        argv: vec![
            "sh".into(),
            "-c".into(),
            format!("printf released > '{}'", path.display()),
        ],
        env: Default::default(),
    }
}

#[test]
fn child_initialization_waits_for_durable_api_association() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("released");
    let association = RecordingAssociation {
        identities: Mutex::new(vec![]),
        fail: false,
    };
    let launcher = DurableSpawn::new();
    let outcome = launcher
        .spawn_persisted("incarnation-a", &marker_command(&marker), &association)
        .unwrap();
    assert!(matches!(
        outcome,
        DurableSpawnOutcome::Uncertain {
            initialization_acknowledged: true,
            ..
        }
    ));
    for _ in 0..50 {
        if marker.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(marker.exists());
    let identities = association.identities.lock().unwrap();
    assert_eq!(identities.len(), 1);
    assert_eq!(identities[0].role, "api");
    assert!(!identities[0].boot_id.is_empty());
    assert!(identities[0].start_ticks > 0);
}

#[test]
fn ambiguous_association_is_not_retried_under_same_incarnation() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("not-released");
    let association = RecordingAssociation {
        identities: Mutex::new(vec![]),
        fail: true,
    };
    let launcher = DurableSpawn::new();
    let first = launcher
        .spawn_persisted("incarnation-a", &marker_command(&marker), &association)
        .unwrap();
    let pid = match first {
        DurableSpawnOutcome::Uncertain {
            handle,
            initialization_acknowledged: false,
            ..
        } => handle.pid,
        other => panic!("unexpected outcome: {other:?}"),
    };
    assert!(!marker.exists());
    assert!(launcher
        .spawn_persisted("incarnation-a", &marker_command(&marker), &association)
        .is_err());
    assert_eq!(association.identities.lock().unwrap().len(), 1);
    drop(launcher);
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(std::path::Path::new(&format!("/proc/{pid}")).exists());
    assert!(!marker.exists(), "gate EOF must not initialize the child");
    nix::sys::signal::killpg(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();
}

#[test]
fn gate_works_when_pipe_descriptors_are_high() {
    let held: Vec<_> = (0..96)
        .map(|_| std::fs::File::open("/dev/null").unwrap())
        .collect();
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("released-high-fd");
    let association = RecordingAssociation {
        identities: Mutex::new(vec![]),
        fail: false,
    };
    let launcher = DurableSpawn::new();
    let outcome = launcher
        .spawn_persisted(
            "incarnation-high-fd",
            &marker_command(&marker),
            &association,
        )
        .unwrap();
    assert!(matches!(
        outcome,
        DurableSpawnOutcome::Uncertain {
            initialization_acknowledged: true,
            ..
        }
    ));
    for _ in 0..50 {
        if marker.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(marker.exists());
    drop(held);
}

#[test]
fn gate_eof_never_acknowledges_initialization() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("eof-must-not-release");
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("eof_parent_fixture")
        .arg("--nocapture")
        .env("MLLM_EOF_MARKER", &marker)
        .status()
        .unwrap();
    assert!(status.success());
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(
        !marker.exists(),
        "EOF is not an initialization acknowledgment"
    );
}

#[test]
fn eof_parent_fixture() {
    let Ok(marker) = std::env::var("MLLM_EOF_MARKER") else {
        return;
    };
    let association = RecordingAssociation {
        identities: Mutex::new(vec![]),
        fail: true,
    };
    let launcher = DurableSpawn::new();
    let _ = launcher
        .spawn_persisted(
            "eof-fixture",
            &marker_command(std::path::Path::new(&marker)),
            &association,
        )
        .unwrap();
    std::process::exit(0);
}
