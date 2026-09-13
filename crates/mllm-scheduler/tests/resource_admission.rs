use mllm_domain::resources::{
    Allocation, DeviceClaim, LedgerSnapshot, MemoryLimit, MemoryObservation, PhaseFootprint,
    ResidentFloor, ResourcePhase, Sharing,
};
use mllm_scheduler::residency::*;

#[test]
fn shared_claims_can_overlap() {
    let shared = DeviceClaim {
        device: "gpu:0".into(),
        sharing: Sharing::Shared,
    };
    let exclusive = DeviceClaim {
        device: "gpu:0".into(),
        sharing: Sharing::Exclusive,
    };
    assert!(!claims_conflict(
        std::slice::from_ref(&shared),
        std::slice::from_ref(&shared)
    ));
    assert!(claims_conflict(
        std::slice::from_ref(&shared),
        std::slice::from_ref(&exclusive)
    ));
    assert!(claims_conflict(&[exclusive], &[shared]));
}

#[test]
fn invalid_and_duplicate_allocations_are_rejected() {
    let a = Allocation {
        domain: "system".into(),
        bytes: 10,
        host_kv_bytes: 11,
    };
    let mut f = PhaseFootprint {
        phase: ResourcePhase::Ready,
        allocations: vec![a],
        devices: vec![],
    };
    assert_eq!(validate_footprint(&f), Err(ResourceError::Invalid));
    f.allocations[0].host_kv_bytes = 0;
    f.allocations.push(f.allocations[0].clone());
    assert_eq!(validate_footprint(&f), Err(ResourceError::Invalid));
    f.allocations.pop();
    f.allocations[0].bytes = -1;
    assert_eq!(validate_footprint(&f), Err(ResourceError::Invalid));
}

#[test]
fn wake_replaces_parked_residue() {
    let footprint = |phase, bytes| PhaseFootprint {
        phase,
        allocations: vec![Allocation {
            domain: "system".into(),
            bytes,
            host_kv_bytes: 0,
        }],
        devices: vec![],
    };
    let snapshot = LedgerSnapshot {
        epoch: 7,
        owners: [
            ("a".into(), footprint(ResourcePhase::Ready, 50)),
            ("b".into(), footprint(ResourcePhase::Parked, 10)),
        ]
        .into(),
    };
    let observations = [MemoryObservation {
        domain: "system".into(),
        capacity_bytes: 128,
        available_bytes: 60,
        sampled_at_ms: 1_000,
    }];
    let limits = [MemoryLimit {
        domain: "system".into(),
        managed_bytes: 96,
        free_reserve_bytes: 12,
        host_kv_bytes: None,
        parked_bytes: None,
    }];
    let floors = [("a", 50), ("b", 10)].map(|(owner, bytes)| ResidentFloor {
        owner: owner.into(),
        domain: "system".into(),
        bytes,
        sampled_at_ms: 1_000,
    });
    assert_eq!(
        admit_phase(
            &snapshot,
            "b",
            &footprint(ResourcePhase::Wake, 46),
            AdmissionContext::new(&observations, &limits, 1_001, 2_000, 4)
                .with_resident_floors(&floors)
        ),
        Ok(())
    );
    assert_eq!(
        admit_phase(
            &snapshot,
            "b",
            &footprint(ResourcePhase::Wake, 47),
            AdmissionContext::new(&observations, &limits, 1_001, 2_000, 4)
                .with_resident_floors(&floors)
        ),
        Err(ResourceError::Insufficient)
    );
}

#[test]
fn reservation_slack_is_not_physical_credit() {
    let f = |phase, bytes| PhaseFootprint {
        phase,
        allocations: vec![Allocation {
            domain: "system".into(),
            bytes,
            host_kv_bytes: 0,
        }],
        devices: vec![],
    };
    let state = LedgerSnapshot {
        epoch: 1,
        owners: [("a".into(), f(ResourcePhase::Parked, 48))].into(),
    };
    let obs = [MemoryObservation {
        domain: "system".into(),
        capacity_bytes: 128,
        available_bytes: 32,
        sampled_at_ms: 100,
    }];
    let limits = [MemoryLimit {
        domain: "system".into(),
        managed_bytes: 96,
        free_reserve_bytes: 16,
        host_kv_bytes: None,
        parked_bytes: None,
    }];
    let floors = [ResidentFloor {
        owner: "a".into(),
        domain: "system".into(),
        bytes: 8,
        sampled_at_ms: 100,
    }];
    for context in [
        AdmissionContext::new(&obs, &limits, 101, 60, 4),
        AdmissionContext::new(&obs, &limits, 101, 60, 4).with_resident_floors(&floors),
    ] {
        assert_eq!(
            admit_phase(&state, "a", &f(ResourcePhase::Wake, 48), context),
            Err(ResourceError::Insufficient)
        );
        assert_eq!(
            admit_phase(&state, "b", &f(ResourcePhase::Cold, 1), context),
            Err(ResourceError::Insufficient)
        );
    }
    for bad in [
        ResidentFloor {
            sampled_at_ms: 99,
            ..floors[0].clone()
        },
        ResidentFloor {
            bytes: 49,
            ..floors[0].clone()
        },
        ResidentFloor {
            bytes: -1,
            ..floors[0].clone()
        },
    ] {
        assert_eq!(
            admit_phase(
                &state,
                "a",
                &f(ResourcePhase::Wake, 48),
                AdmissionContext::new(&obs, &limits, 101, 60, 4).with_resident_floors(&[bad])
            ),
            Err(ResourceError::Invalid)
        );
    }
    assert_eq!(
        admit_phase(
            &state,
            "a",
            &f(ResourcePhase::Wake, 48),
            AdmissionContext::new(&obs, &limits, 101, 60, 4)
                .with_resident_floors(&[floors[0].clone(), floors[0].clone()])
        ),
        Err(ResourceError::Invalid)
    );
}

#[test]
fn installed_capacity_is_not_available_memory() {
    let next = PhaseFootprint {
        phase: ResourcePhase::Cold,
        allocations: vec![Allocation {
            domain: "system".into(),
            bytes: 30,
            host_kv_bytes: 0,
        }],
        devices: vec![],
    };
    let snapshot = LedgerSnapshot {
        epoch: 0,
        owners: Default::default(),
    };
    let mut obs = [MemoryObservation {
        domain: "system".into(),
        capacity_bytes: 128,
        available_bytes: 20,
        sampled_at_ms: 100,
    }];
    let limits = [MemoryLimit {
        domain: "system".into(),
        managed_bytes: 96,
        free_reserve_bytes: 12,
        host_kv_bytes: None,
        parked_bytes: None,
    }];
    assert_eq!(
        admit_phase(
            &snapshot,
            "a",
            &next,
            AdmissionContext::new(&obs, &limits, 101, 60, 4)
        ),
        Err(ResourceError::Insufficient)
    );
    obs[0].sampled_at_ms = 0;
    assert_eq!(
        admit_phase(
            &snapshot,
            "a",
            &next,
            AdmissionContext::new(&obs, &limits, 101, 60, 4)
        ),
        Err(ResourceError::StaleObservation)
    );
}
