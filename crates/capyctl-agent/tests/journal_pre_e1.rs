//! Owner decision 2026-09-22: host journals written before ADR 0014 (E1) stay
//! readable. A journaled command is digest-bound and is never re-signed, so a
//! pre-E1 `LaunchSingle` must decode after the upgrade exactly as it was
//! accepted, or its launch would no longer be this host's own.

use capyctl_agent::journal::{Acceptance, HostJournal, JournalError, LocalExecutionPolicy};
use capyctl_domain::group::{CommandIdentity, MemberKey};
use capyctl_protocol::execution::{MemberAction, MemberCommand, SingleLaunchPlan};
use std::os::unix::fs::PermissionsExt;

struct Policy;
impl LocalExecutionPolicy for Policy {
    fn authorize(&self, _: &MemberCommand) -> Result<(), JournalError> {
        Ok(())
    }
    fn render_launch(
        &self,
        _: &MemberCommand,
    ) -> Result<capyctl_agent::journal::ApprovedLaunch, JournalError> {
        Err(JournalError::Unauthorized)
    }
}

fn directory() -> tempfile::TempDir {
    let d = tempfile::Builder::new()
        .prefix("capyctl-journal-")
        .tempdir_in(std::env::var("HOME").unwrap())
        .unwrap();
    std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    d
}

/// The deployment a pre-E1 controller sent: the pre-E1 fixture had no
/// `engine_config` (`git show HEAD:crates/capyctl-config/tests/fixtures/f2-deployment.json`),
/// canonicalized the way the controller rendered it.
fn pre_e1_deployment() -> String {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/f2-deployment.json"
    ))
    .unwrap();
    let mut deployment = fixture["deployment"].clone();
    deployment.as_object_mut().unwrap().remove("engine_config");
    capyctl_config::parse_strict(
        capyctl_config::ConfigKind::Deployment,
        &deployment.to_string(),
    )
    .unwrap()
    .to_string()
}

// T33 T13
#[test]
fn a_pre_e1_launch_command_re_decodes_from_the_journal_unchanged() {
    let d = directory();
    let journal = HostJournal::open(d.path(), "controller", "host").unwrap();
    let session = journal.connect().unwrap();
    let mut command = MemberCommand {
        identity: CommandIdentity {
            controller_id: "controller".into(),
            member: MemberKey {
                host_id: "host".into(),
                member_id: "head".into(),
            },
            deployment_id: "deployment".into(),
            operation_id: "operation".into(),
            command_id: "pre-e1".into(),
            step_id: "pre-e1".into(),
            generation: 1,
            revision: 1,
            deadline_ms: 30_000,
            payload_digest: [0; 32],
            expected_state: "reserved".into(),
            profile_fingerprint: "vllm-build-1".into(),
            instance_index: 0,
        },
        action: MemberAction::LaunchSingle(SingleLaunchPlan {
            deployment_config: pre_e1_deployment(),
            profile_name: "local".into(),
            checkpoint_fingerprint: "sha256:model".into(),
            host_policy_fingerprint: "b".repeat(64),
            binding_id: "01K00000000000000000000001".into(),
            incarnation: "01K00000000000000000000002".into(),
            grant_id: "01K00000000000000000000003".into(),
            service_port: 8100,
            issued_at_ms: 1,
            coordinator_session_id: "01K00000000000000000000004".into(),
            // Journaled before WE3: no recorded checkpoint.
            checkpoint_digest: String::new(),
            checkpoint_weights_bytes: None,
            checkpoint_state_slot_bytes: None,
            checkpoint_layout: None,
            startup_bytes: None,
        }),
    };
    command.identity.payload_digest = command.canonical_digest();
    assert!(matches!(
        journal.accept(session, &command, 10, &Policy).unwrap(),
        Acceptance::Fresh(_)
    ));
    drop(journal);
    // The upgraded agent reopens the same journal and reads the command back
    // through the full decode: wire, strict deployment parse and digest.
    let journal = HostJournal::open(d.path(), "controller", "host").unwrap();
    let retained = journal.retained_command("pre-e1").unwrap();
    assert_eq!(retained, command);
    retained.verify_digest().unwrap();
    // An exact replay of it is still a replay, never fresh work.
    let session = journal.connect().unwrap();
    assert!(matches!(
        journal.accept(session, &command, 20, &Policy).unwrap(),
        Acceptance::Replay(_)
    ));
}
