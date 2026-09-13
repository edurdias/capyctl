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
    assert!(matches!(outcome, DurableSpawnOutcome::Released { .. }));
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
    assert!(matches!(first, DurableSpawnOutcome::Uncertain { .. }));
    assert!(!marker.exists());
    assert!(launcher
        .spawn_persisted("incarnation-a", &marker_command(&marker), &association)
        .is_err());
    assert_eq!(association.identities.lock().unwrap().len(), 1);
}
