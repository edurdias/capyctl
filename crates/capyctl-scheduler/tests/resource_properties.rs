use capyctl_domain::resources::*;
use capyctl_scheduler::residency::*;
use proptest::prelude::*;

proptest! {
    #[test]
    fn replacing_owner_charges_peak_once(old in 0i64..50, peak in 50i64..100) {
        let f = |phase, bytes| PhaseFootprint { phase,
            allocations: vec![Allocation { domain: "system".into(), bytes, host_kv_bytes: 0 }],
            devices: vec![] };
        let state = LedgerSnapshot { epoch: 1, owners: [
            ("a".into(), f(ResourcePhase::Parked, old)),
        ].into() };
        let obs = [MemoryObservation { domain: "system".into(), capacity_bytes: 128,
            available_bytes: 128 - old, sampled_at_ms: 100 }];
        let limits = [MemoryLimit { domain: "system".into(), managed_bytes: peak,
            free_reserve_bytes: 12, reserve_absorbs_unmanaged: false, host_kv_bytes: None, parked_bytes: None }];
        let floors = [ResidentFloor { owner: "a".into(), domain: "system".into(),
            bytes: old, sampled_at_ms: 100 }];
        prop_assert_eq!(admit_phase(&state, "a", &f(ResourcePhase::Wake, peak),
            AdmissionContext::new(&obs, &limits, 101, 60, 4).with_resident_floors(&floors)), Ok(()));
        let too_large = f(ResourcePhase::Wake, peak + 1);
        prop_assert_eq!(admit_phase(&state, "a", &too_large,
            AdmissionContext::new(&obs, &limits, 101, 60, 4)), Err(ResourceError::Insufficient));
    }
}

#[test]
fn categories_and_domains_do_not_create_capacity() {
    let f = |domain: &str, bytes, kv| PhaseFootprint {
        phase: ResourcePhase::Ready,
        allocations: vec![Allocation {
            domain: domain.into(),
            bytes,
            host_kv_bytes: kv,
        }],
        devices: vec![],
    };
    let state = LedgerSnapshot {
        epoch: 1,
        owners: [("a".into(), f("system", 40, 9))].into(),
    };
    let obs = [MemoryObservation {
        domain: "system".into(),
        capacity_bytes: 128,
        available_bytes: 88,
        sampled_at_ms: 100,
    }];
    let limits = [MemoryLimit {
        domain: "system".into(),
        managed_bytes: 96,
        free_reserve_bytes: 12,
        reserve_absorbs_unmanaged: false,
        host_kv_bytes: Some(16),
        parked_bytes: None,
    }];
    for (next, error) in [
        (f("system", 40, 8), ResourceError::CategoryLimit),
        (f("unmapped-vram", 1, 0), ResourceError::UnknownDomain),
        (f("system", i64::MAX, 0), ResourceError::Invalid),
    ] {
        assert_eq!(
            admit_phase(
                &state,
                "b",
                &next,
                AdmissionContext::new(&obs, &limits, 101, 60, 4)
            ),
            Err(error)
        );
    }
}

#[test]
fn recipe_peaks_cover_each_adjacent_phase() {
    let f = |phase, bytes| PhaseFootprint {
        phase,
        allocations: vec![Allocation {
            domain: "system".into(),
            bytes,
            host_kv_bytes: 0,
        }],
        devices: vec![],
    };
    let mut recipe = RecipeFootprints {
        cold: f(ResourcePhase::Cold, 80),
        ready: f(ResourcePhase::Ready, 60),
        parking: f(ResourcePhase::Parking, 65),
        parked: f(ResourcePhase::Parked, 8),
        wake: f(ResourcePhase::Wake, 70),
    };
    assert_eq!(validate_recipe(&recipe), Ok(()));
    recipe.parking.allocations[0].bytes = 59;
    assert_eq!(validate_recipe(&recipe), Err(ResourceError::Invalid));
    recipe.parking.allocations[0].bytes = 65;
    recipe.wake.allocations[0].domain = "wrong-domain".into();
    assert_eq!(validate_recipe(&recipe), Err(ResourceError::Invalid));
}

#[test]
fn parked_and_timestamp_limits_are_enforced() {
    let state = LedgerSnapshot {
        epoch: 1,
        owners: Default::default(),
    };
    let next = PhaseFootprint {
        phase: ResourcePhase::Parked,
        allocations: vec![Allocation {
            domain: "system".into(),
            bytes: 10,
            host_kv_bytes: 0,
        }],
        devices: vec![],
    };
    let mut obs = [MemoryObservation {
        domain: "system".into(),
        capacity_bytes: 128,
        available_bytes: 128,
        sampled_at_ms: 100,
    }];
    let mut limits = [MemoryLimit {
        domain: "system".into(),
        managed_bytes: 96,
        free_reserve_bytes: 12,
        reserve_absorbs_unmanaged: false,
        host_kv_bytes: None,
        parked_bytes: Some(9),
    }];
    assert_eq!(
        admit_phase(
            &state,
            "a",
            &next,
            AdmissionContext::new(&obs, &limits, 101, 60, 4)
        ),
        Err(ResourceError::CategoryLimit)
    );
    limits[0].parked_bytes = Some(10);
    assert_eq!(
        admit_phase(
            &state,
            "a",
            &next,
            AdmissionContext::new(&obs, &limits, 101, 60, 0)
        ),
        Err(ResourceError::CategoryLimit)
    );
    obs[0].sampled_at_ms = 102;
    assert_eq!(
        admit_phase(
            &state,
            "a",
            &next,
            AdmissionContext::new(&obs, &limits, 101, 60, 4)
        ),
        Err(ResourceError::StaleObservation)
    );
}

#[test]
fn spare_system_memory_cannot_cover_a_full_discrete_gpu() {
    let state = LedgerSnapshot {
        epoch: 0,
        owners: Default::default(),
    };
    let next = PhaseFootprint {
        phase: ResourcePhase::Cold,
        allocations: vec![
            Allocation {
                domain: "system".into(),
                bytes: 5,
                host_kv_bytes: 0,
            },
            Allocation {
                domain: "gpu-memory:0".into(),
                bytes: 50,
                host_kv_bytes: 0,
            },
        ],
        devices: vec![],
    };
    let obs = [
        MemoryObservation {
            domain: "system".into(),
            capacity_bytes: 128,
            available_bytes: 128,
            sampled_at_ms: 100,
        },
        MemoryObservation {
            domain: "gpu-memory:0".into(),
            capacity_bytes: 48,
            available_bytes: 48,
            sampled_at_ms: 100,
        },
    ];
    let limits = [
        MemoryLimit {
            domain: "system".into(),
            managed_bytes: 96,
            free_reserve_bytes: 12,
            reserve_absorbs_unmanaged: false,
            host_kv_bytes: None,
            parked_bytes: None,
        },
        MemoryLimit {
            domain: "gpu-memory:0".into(),
            managed_bytes: 40,
            free_reserve_bytes: 4,
            reserve_absorbs_unmanaged: false,
            host_kv_bytes: None,
            parked_bytes: None,
        },
    ];
    assert_eq!(
        admit_phase(
            &state,
            "a",
            &next,
            AdmissionContext::new(&obs, &limits, 101, 60, 4)
        ),
        Err(ResourceError::Insufficient)
    );
}
