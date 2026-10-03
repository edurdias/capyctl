use capyctl_domain::completion::*;
use capyctl_domain::resources::*;

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
    // An api process alone is a single-process group (ADR 0023 §6), covered by
    // `a_single_process_launch_completes_on_its_api_identity`; it never stands
    // in for a group that had a worker.
    duplicate.identities.truncate(1);
    assert_eq!(
        verify_completion(&expected, &duplicate, 151, 60),
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

// T41 (ADR 0023 §6): TensorFold serves from one process, so a launch whose
// recorded group is the api process alone completes on that same process.
#[test]
fn a_single_process_launch_completes_on_its_api_identity() {
    let (mut expected, mut evidence) = fixture();
    expected.identities.truncate(1);
    evidence.identities.truncate(1);
    verify_completion(&expected, &evidence, 151, 60).unwrap();
    let mut worker_only = evidence.clone();
    worker_only.identities[0].role = "worker-0".into();
    expected.identities[0].role = "worker-0".into();
    assert_eq!(
        verify_completion(&expected, &worker_only, 151, 60),
        Err(CompletionError::RuntimeChanged),
        "a group without its api process never completes"
    );
}

fn member(role: &str, pid: u32) -> ProcessIdentity {
    ProcessIdentity {
        role: role.into(),
        pid,
        boot_id: "boot-a".into(),
        start_ticks: u64::from(pid) + 1,
    }
}

// ADR 0027: a helper (a process the engine's own processes did not start
// directly, such as a compile worker) is recorded, but only the engine's own
// processes decide whether the recorded engine is still the one running.
#[test]
fn a_helper_may_be_gone_but_an_engine_process_may_not() {
    let recorded = vec![
        member("api", 10),
        member("worker-0", 11),
        member("helper-0", 12),
        member("helper-1", 13),
    ];
    assert!(member("helper-0", 12).is_helper());
    assert!(!member("worker-0", 11).is_helper());
    assert!(!member("api", 10).is_helper());
    assert!(
        !member("helper-", 14).is_helper(),
        "a bare prefix is no role"
    );
    assert_eq!(
        engine_members(&recorded),
        vec![member("api", 10), member("worker-0", 11)]
    );

    assert!(same_engine(&recorded, &recorded));
    assert!(
        same_engine(&recorded, &recorded[..2]),
        "every helper gone is still the engine"
    );
    assert!(same_engine(
        &recorded,
        &[
            member("api", 10),
            member("worker-0", 11),
            member("helper-1", 13)
        ]
    ));
    assert!(
        !same_engine(&recorded, &[member("api", 10), member("helper-0", 12)]),
        "a worker gone is not the engine"
    );
    assert!(
        !same_engine(&recorded, &[member("worker-0", 11)]),
        "the api gone is not the engine"
    );
    assert!(
        same_engine(
            &recorded,
            &[
                member("api", 10),
                member("worker-0", 11),
                member("helper-2", 20)
            ]
        ),
        "a helper started later does not change the engine"
    );
    assert!(
        !same_engine(
            &recorded,
            &[
                member("api", 10),
                member("worker-0", 11),
                member("worker-1", 20)
            ]
        ),
        "an engine process outside the recorded group is never the engine's"
    );
    assert!(
        !same_engine(&recorded, &[member("api", 10), member("worker-0", 21)]),
        "a replaced worker is another engine"
    );
    assert!(!same_engine(&[], &[]));
}

// ADR 0027: the Ready proof still needs the recorded group exactly; only later
// liveness proofs let a helper be gone.
#[test]
fn a_launch_with_helpers_completes_only_on_its_whole_group() {
    let (mut expected, mut evidence) = fixture();
    expected.identities.push(member("helper-0", 102));
    evidence.identities.push(member("helper-0", 102));
    verify_completion(&expected, &evidence, 151, 60).unwrap();
    evidence.identities.pop();
    assert_eq!(
        verify_completion(&expected, &evidence, 151, 60),
        Err(CompletionError::RuntimeChanged)
    );
}
