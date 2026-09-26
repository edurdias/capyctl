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
fn admission_enforces_device_claim_conflicts() {
    let footprint = |owner_sharing| PhaseFootprint {
        phase: ResourcePhase::Ready,
        allocations: vec![Allocation {
            domain: "system".into(),
            bytes: 10,
            host_kv_bytes: 0,
        }],
        devices: vec![DeviceClaim {
            device: "gpu:0".into(),
            sharing: owner_sharing,
        }],
    };
    let observations = [MemoryObservation {
        domain: "system".into(),
        capacity_bytes: 100,
        available_bytes: 90,
        sampled_at_ms: 100,
    }];
    let limits = [MemoryLimit {
        domain: "system".into(),
        managed_bytes: 100,
        free_reserve_bytes: 0,
        host_kv_bytes: None,
        parked_bytes: None,
    }];
    let context = || AdmissionContext::new(&observations, &limits, 101, 60, 4);

    let exclusive_snapshot = LedgerSnapshot {
        epoch: 0,
        owners: [("existing".into(), footprint(Sharing::Exclusive))].into(),
    };
    for candidate_sharing in [Sharing::Shared, Sharing::Exclusive] {
        assert_eq!(
            admit_phase(
                &exclusive_snapshot,
                "candidate",
                &footprint(candidate_sharing),
                context(),
            ),
            Err(ResourceError::DeviceConflict)
        );
    }

    let shared_snapshot = LedgerSnapshot {
        epoch: 0,
        owners: [("existing".into(), footprint(Sharing::Shared))].into(),
    };
    assert_eq!(
        admit_phase(
            &shared_snapshot,
            "candidate",
            &footprint(Sharing::Shared),
            context(),
        ),
        Ok(())
    );
}

#[test]
fn aggregate_resident_floors_cannot_exceed_observed_usage() {
    let footprint = |bytes| PhaseFootprint {
        phase: ResourcePhase::Ready,
        allocations: vec![Allocation {
            domain: "system".into(),
            bytes,
            host_kv_bytes: 0,
        }],
        devices: vec![],
    };
    let snapshot = LedgerSnapshot {
        epoch: 0,
        owners: [("a".into(), footprint(40)), ("b".into(), footprint(40))].into(),
    };
    let observations = [MemoryObservation {
        domain: "system".into(),
        capacity_bytes: 100,
        available_bytes: 30,
        sampled_at_ms: 100,
    }];
    let limits = [MemoryLimit {
        domain: "system".into(),
        managed_bytes: 100,
        free_reserve_bytes: 0,
        host_kv_bytes: None,
        parked_bytes: None,
    }];
    let floors = |a, b| {
        [("a", a), ("b", b)].map(|(owner, bytes)| ResidentFloor {
            owner: owner.into(),
            domain: "system".into(),
            bytes,
            sampled_at_ms: 100,
        })
    };
    let candidate = footprint(0);

    let boundary = floors(35, 35);
    assert_eq!(
        admit_phase(
            &snapshot,
            "candidate",
            &candidate,
            AdmissionContext::new(&observations, &limits, 101, 60, 4)
                .with_resident_floors(&boundary),
        ),
        Ok(())
    );

    let inconsistent = floors(36, 35);
    assert_eq!(
        admit_phase(
            &snapshot,
            "candidate",
            &candidate,
            AdmissionContext::new(&observations, &limits, 101, 60, 4)
                .with_resident_floors(&inconsistent),
        ),
        Err(ResourceError::Invalid)
    );
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

#[test]
fn two_proposals_cannot_spend_one_epoch() {
    let snapshot = LedgerSnapshot {
        epoch: 4,
        owners: Default::default(),
    };
    let next = PhaseFootprint {
        phase: ResourcePhase::Cold,
        allocations: vec![Allocation {
            domain: "system".into(),
            bytes: 60,
            host_kv_bytes: 0,
        }],
        devices: vec![],
    };
    let obs = [MemoryObservation {
        domain: "system".into(),
        capacity_bytes: 128,
        available_bytes: 128,
        sampled_at_ms: 100,
    }];
    let limits = [MemoryLimit {
        domain: "system".into(),
        managed_bytes: 96,
        free_reserve_bytes: 12,
        host_kv_bytes: None,
        parked_bytes: None,
    }];
    let a = propose_phase(
        &snapshot,
        "a",
        &next,
        AdmissionContext::new(&obs, &limits, 101, 60, 4),
    )
    .unwrap();
    let b = propose_phase(
        &snapshot,
        "b",
        &next,
        AdmissionContext::new(&obs, &limits, 101, 60, 4),
    )
    .unwrap();
    let updated = apply_proposal_to_snapshot(&snapshot, &a, 102).unwrap();
    assert_eq!(
        apply_proposal_to_snapshot(&updated, &b, 102),
        Err(ResourceError::StaleEpoch)
    );
    assert_eq!(
        apply_proposal_to_snapshot(&snapshot, &a, 161),
        Err(ResourceError::StaleObservation)
    );
}

// T16 T26: found live on the 16 GB discrete-GPU laptop host. After a
// host_backed park, host RAM sat below the system domain's free reserve, and
// a wake of another model (which releases its own host copy there) was held
// until its deadline. A domain on which the candidate adds nothing beyond
// what its own processes hold is not judged on free memory: refusing it
// cannot restore the reserve. The managed limit still applies.
#[test]
fn a_domain_the_candidate_adds_nothing_to_is_not_judged_on_free_memory() {
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
        owners: [
            ("a".into(), f(ResourcePhase::Parked, 20)),
            ("b".into(), f(ResourcePhase::Parked, 30)),
        ]
        .into(),
    };
    // 10 free, below the 16 reserve.
    let obs = [MemoryObservation {
        domain: "system".into(),
        capacity_bytes: 128,
        available_bytes: 10,
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
        bytes: 20,
        sampled_at_ms: 100,
    }];
    let context = AdmissionContext::new(&obs, &limits, 101, 60, 4).with_resident_floors(&floors);
    assert_eq!(
        admit_phase(&state, "a", &f(ResourcePhase::Wake, 12), context),
        Ok(())
    );
    // One byte beyond its own floor is judged as before.
    assert_eq!(
        admit_phase(&state, "a", &f(ResourcePhase::Wake, 21), context),
        Err(ResourceError::Insufficient)
    );
    // Without the floor it is judged as before.
    assert_eq!(
        admit_phase(
            &state,
            "a",
            &f(ResourcePhase::Wake, 12),
            AdmissionContext::new(&obs, &limits, 101, 60, 4)
        ),
        Err(ResourceError::Insufficient)
    );
    // The managed limit still applies: 30 + 67 > 96.
    assert_eq!(
        admit_phase(&state, "a", &f(ResourcePhase::Wake, 67), context),
        Err(ResourceError::Insufficient)
    );
}
