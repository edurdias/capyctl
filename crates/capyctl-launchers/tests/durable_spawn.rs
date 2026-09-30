use std::sync::{Arc, Mutex};

use capyctl_adapters::traits::RenderedCommand;
use capyctl_domain::completion::ProcessIdentity;
use capyctl_launchers::{AssociationError, DurableSpawn, DurableSpawnOutcome, LaunchAssociation};

#[test]
fn protected_descriptors_are_private_sealed_bounded_and_close_on_drop() {
    use capyctl_launchers::ProtectedLaunchDescriptors;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let descriptors =
        ProtectedLaunchDescriptors::new(b"{}", b"inference-secret", b"admin-secret").unwrap();
    let numbers = descriptors.numbers();
    let inodes = numbers.map(|fd| {
        std::fs::metadata(format!("/proc/self/fd/{fd}"))
            .unwrap()
            .ino()
    });
    for (fd, expected) in
        numbers
            .into_iter()
            .zip([b"{}".as_slice(), b"inference-secret", b"admin-secret"])
    {
        assert!(fd > 9, "must not collide with initialization gate");
        let path = format!("/proc/self/fd/{fd}");
        let metadata = std::fs::metadata(&path).unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read(&path).unwrap(), expected);
        assert_eq!(
            nix::unistd::lseek(fd, 0, nix::unistd::Whence::SeekCur).unwrap(),
            0
        );
        assert_ne!(
            nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_GETFD).unwrap()
                & nix::fcntl::FdFlag::FD_CLOEXEC.bits(),
            0
        );
        let seals = nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_GET_SEALS).unwrap();
        assert_ne!(seals & nix::fcntl::SealFlag::F_SEAL_WRITE.bits(), 0);
    }
    drop(descriptors);
    for (fd, inode) in numbers.into_iter().zip(inodes) {
        // Parallel tests can reuse a closed descriptor number immediately.
        assert!(!std::fs::metadata(format!("/proc/self/fd/{fd}"))
            .is_ok_and(|metadata| metadata.ino() == inode));
    }
    for (launch, inference, admin) in [
        (vec![], b"one".to_vec(), b"two".to_vec()),
        (vec![b'x'; 65537], b"one".to_vec(), b"two".to_vec()),
        (b"{}".to_vec(), vec![], b"two".to_vec()),
        (b"{}".to_vec(), b"same".to_vec(), b"same".to_vec()),
        (b"{}".to_vec(), b"bad\nkey".to_vec(), b"two".to_vec()),
        (b"{}".to_vec(), vec![b'x'; 4097], b"two".to_vec()),
    ] {
        assert!(ProtectedLaunchDescriptors::new(&launch, &inference, &admin).is_err());
    }
}

#[test]
fn protected_descriptors_reach_only_gated_child_and_association_failure_keeps_gate_closed() {
    use capyctl_launchers::ProtectedLaunchDescriptors;
    for fail in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("descriptor-content");
        let descriptors =
            ProtectedLaunchDescriptors::new(b"{}", b"inference-secret", b"admin-secret").unwrap();
        let [_, inference, _] = descriptors.numbers();
        let command = RenderedCommand {
            argv: vec![
                "sh".into(),
                "-c".into(),
                format!("cat /proc/self/fd/{inference} > '{}'", marker.display()),
            ],
            env: Default::default(),
        };
        let association = RecordingAssociation {
            identities: Mutex::new(vec![]),
            fail,
        };
        let launcher = DurableSpawn::new();
        let outcome = launcher
            .spawn_protected("protected", &command, &descriptors, &association)
            .unwrap();
        let DurableSpawnOutcome::Uncertain {
            handle,
            initialization_acknowledged,
            ..
        } = outcome;
        assert_eq!(initialization_acknowledged, !fail);
        drop(descriptors);
        if fail {
            // The launch was never released, so the launcher has already disposed of
            // the child: the descriptors reached nothing and no process is left.
            assert!(!marker.exists());
            assert!(
                !std::path::Path::new(&format!("/proc/{}", handle.pid)).exists(),
                "gated child still present"
            );
        } else {
            for _ in 0..50 {
                if std::fs::read(&marker).is_ok_and(|bytes| bytes == b"inference-secret") {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert_eq!(std::fs::read(&marker).unwrap(), b"inference-secret");
        }
        assert!(launcher
            .spawn_protected(
                "protected",
                &command,
                &ProtectedLaunchDescriptors::new(b"{}", b"new-inference", b"new-admin").unwrap(),
                &association
            )
            .is_err());
    }
}

/// A protected spawn through the ordinary owned process tools: the gated child
/// inherits the three descriptors and can read their contents, exactly as the
/// direct `DurableSpawn::spawn_protected` path behaves. SPEC §13.3: credentials
/// ride protected descriptors, never argv.
// T12
#[test]
fn owned_process_launch_spawns_with_protected_descriptors() {
    use capyctl_adapters::traits::OwnedProcessLaunch;
    use capyctl_launchers::{DurableProcessLaunch, ProtectedLaunchDescriptors};
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("descriptor-content");
    let descriptors = ProtectedLaunchDescriptors::new(
        b"{\"private\":true}",
        b"inference-secret",
        b"admin-secret",
    )
    .unwrap();
    let [launch_fd, inference_fd, admin_fd] = descriptors.numbers();
    let command = RenderedCommand {
        argv: vec![
            "sh".into(),
            "-c".into(),
            format!(
                "cat /proc/self/fd/{launch_fd} /proc/self/fd/{inference_fd} /proc/self/fd/{admin_fd} > '{}'",
                marker.display()
            ),
        ],
        env: Default::default(),
    };
    let tools: &dyn OwnedProcessLaunch =
        &DurableProcessLaunch::new(Arc::new(RecordingAssociation {
            identities: Mutex::new(vec![]),
            fail: false,
        }));
    let identity = tools
        .spawn_durable_protected("protected-tools", &command, &descriptors)
        .unwrap();
    assert_eq!(identity.role, "api");
    drop(descriptors);
    let expected = [
        b"{\"private\":true}".as_slice(),
        b"inference-secret",
        b"admin-secret",
    ]
    .concat();
    for _ in 0..50 {
        if std::fs::read(&marker).is_ok_and(|bytes| bytes == expected) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(std::fs::read(&marker).unwrap(), expected);
}

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
    // An unreleased child is disposed of before the outcome is returned, so the
    // ambiguity leaves no process behind to be reconciled later.
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "gated child still present"
    );
    assert!(!marker.exists(), "gate EOF must not initialize the child");
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
        .env("CAPYCTL_EOF_MARKER", &marker)
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
    let Ok(marker) = std::env::var("CAPYCTL_EOF_MARKER") else {
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
