use mllm_domain::completion::*;
use mllm_domain::resources::*;

fn fixture() -> (CompletionExpectation, CompletionEvidence) {
    let token = TransitionToken {
        deployment_id: "a".into(),
        revision: 1,
        generation: 7,
        operation_id: "op-a".into(),
        step_id: "park-a-1".into(),
    };
    let identities = vec![
        ProcessIdentity {
            role: "api".into(),
            pid: 100,
            boot_id: "boot-a".into(),
            start_ticks: 30,
        },
        ProcessIdentity {
            role: "worker-0".into(),
            pid: 101,
            boot_id: "boot-a".into(),
            start_ticks: 31,
        },
    ];
    let target = PhaseFootprint {
        phase: ResourcePhase::Parked,
        allocations: vec![Allocation {
            domain: "system".into(),
            bytes: 8,
            host_kv_bytes: 0,
        }],
        devices: vec![],
    };
    let expected = CompletionExpectation {
        token: token.clone(),
        identities: identities.clone(),
        target,
        issued_at_ms: 100,
        deadline_ms: 200,
    };
    let evidence = CompletionEvidence {
        token,
        identities,
        observed_at_ms: 150,
        control_receipt: Some("ack-park-a-1".into()),
        milestones: vec![Milestone::Quiesced, Milestone::MemoryReleased],
    };
    (expected, evidence)
}

#[test]
fn qualified_ack_and_unchanged_workers_can_complete_parking() {
    let (expected, evidence) = fixture();
    let verified = verify_completion(&expected, &evidence, 151, 60).unwrap();
    assert_eq!(verified.token(), &expected.token);
    assert_eq!(verified.target(), &expected.target);
}

#[test]
fn stale_tokens_do_not_release_resources() {
    let (expected, evidence) = fixture();
    let mut invalid = Vec::new();
    let mut changed = evidence.clone();
    changed.token.revision += 1;
    invalid.push(changed);
    let mut changed = evidence.clone();
    changed.token.generation += 1;
    invalid.push(changed);
    let mut changed = evidence.clone();
    changed.token.step_id.push('x');
    invalid.push(changed);
    let mut changed = evidence.clone();
    changed.token.operation_id.push('x');
    invalid.push(changed);
    let mut changed = evidence;
    changed.token.deployment_id.push('x');
    invalid.push(changed);
    for changed in invalid {
        assert_eq!(
            verify_completion(&expected, &changed, 151, 60),
            Err(CompletionError::StaleToken)
        );
    }
}

#[test]
fn api_survival_does_not_hide_worker_loss_or_pid_reuse() {
    let (expected, evidence) = fixture();
    let mut missing = evidence.clone();
    missing.identities.pop();
    assert_eq!(
        verify_completion(&expected, &missing, 151, 60),
        Err(CompletionError::RuntimeChanged)
    );
    let mut changed_pid = evidence.clone();
    changed_pid.identities[1].pid += 1;
    assert_eq!(
        verify_completion(&expected, &changed_pid, 151, 60),
        Err(CompletionError::RuntimeChanged)
    );
    let mut reused = evidence.clone();
    reused.identities[1].start_ticks += 1;
    assert_eq!(
        verify_completion(&expected, &reused, 151, 60),
        Err(CompletionError::RuntimeChanged)
    );
    let mut rebooted = evidence.clone();
    rebooted.identities[1].boot_id.push('x');
    assert_eq!(
        verify_completion(&expected, &rebooted, 151, 60),
        Err(CompletionError::RuntimeChanged)
    );
    let mut reordered = evidence.clone();
    reordered.identities.reverse();
    assert!(verify_completion(&expected, &reordered, 151, 60).is_ok());
    let mut duplicate = evidence;
    duplicate.identities.push(duplicate.identities[0].clone());
    assert_eq!(
        verify_completion(&expected, &duplicate, 151, 60),
        Err(CompletionError::RuntimeChanged)
    );
    let mut api_only_expected = expected.clone();
    api_only_expected.identities.truncate(1);
    duplicate.identities.truncate(1);
    assert_eq!(
        verify_completion(&api_only_expected, &duplicate, 151, 60),
        Err(CompletionError::RuntimeChanged)
    );
}

#[test]
fn lost_ack_and_expired_evidence_stay_uncertain() {
    let (expected, evidence) = fixture();
    let mut lost = evidence.clone();
    lost.control_receipt = None;
    assert_eq!(
        verify_completion(&expected, &lost, 151, 60),
        Err(CompletionError::Incomplete)
    );
    assert_eq!(
        verify_completion(&expected, &evidence, 149, 60),
        Err(CompletionError::Expired)
    );
    assert_eq!(
        verify_completion(&expected, &evidence, 201, 60),
        Err(CompletionError::Expired)
    );
    assert_eq!(
        verify_completion(&expected, &evidence, 161, 10),
        Err(CompletionError::Expired)
    );
    let mut old = evidence;
    old.observed_at_ms = 99;
    assert_eq!(
        verify_completion(&expected, &old, 151, 60),
        Err(CompletionError::Expired)
    );
}

#[test]
fn every_restore_milestone_is_required_before_ready() {
    let (mut expected, mut evidence) = fixture();
    expected.target.phase = ResourcePhase::Ready;
    evidence.milestones = vec![
        Milestone::AllocationsRestored,
        Milestone::WeightsUsable,
        Milestone::CacheValid,
        Milestone::ModelUsable,
    ];
    assert!(verify_completion(&expected, &evidence, 151, 60).is_ok());
    for index in 0..evidence.milestones.len() {
        let mut missing = evidence.clone();
        missing.milestones.remove(index);
        assert_eq!(
            verify_completion(&expected, &missing, 151, 60),
            Err(CompletionError::Incomplete)
        );
    }
    let mut duplicate = evidence;
    duplicate.milestones.push(Milestone::ModelUsable);
    assert_eq!(
        verify_completion(&expected, &duplicate, 151, 60),
        Err(CompletionError::Incomplete)
    );
    duplicate.milestones.pop();
    duplicate.milestones.swap(2, 3);
    assert_eq!(
        verify_completion(&expected, &duplicate, 151, 60),
        Err(CompletionError::Incomplete)
    );
}

#[test]
fn invalid_target_footprint_is_rejected() {
    let (mut expected, evidence) = fixture();
    let mut invalid_target = expected.target.clone();
    invalid_target.allocations[0].bytes = -1;
    expected.target = invalid_target;
    assert_eq!(
        verify_completion(&expected, &evidence, 151, 60),
        Err(CompletionError::Invalid)
    );
}

// T24: a zero start identity cannot prove that an owned worker survived.
#[test]
fn missing_process_start_identity_cannot_complete() {
    let (mut expected, mut evidence) = fixture();
    expected.identities[1].start_ticks = 0;
    evidence.identities[1].start_ticks = 0;
    assert_eq!(
        verify_completion(&expected, &evidence, 151, 60),
        Err(CompletionError::RuntimeChanged)
    );
}
