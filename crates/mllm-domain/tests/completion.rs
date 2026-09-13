use mllm_domain::completion::*;
use mllm_domain::resources::*;

fn fixture() -> (CompletionExpectation, CompletionEvidence) {
    let token = TransitionToken {
        deployment_id: "a".into(),
        revision: 1,
        generation: 7,
        operation_id: "op-a".into(),
        step_id: "park-a-1".into(),
        qualification_id: "qualified-recipe-a".into(),
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
