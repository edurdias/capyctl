use capyctl_domain::resources::*;
use capyctl_scheduler::{
    residency::{AdmissionContext, ResourceError},
    sequence::*,
};

fn f(phase: ResourcePhase, bytes: i64) -> PhaseFootprint {
    PhaseFootprint {
        phase,
        allocations: vec![Allocation {
            domain: "system".into(),
            bytes,
            host_kv_bytes: 0,
        }],
        devices: vec![],
    }
}
#[test]
fn park_before_wake_fits_without_cold_restart() {
    let initial = LedgerSnapshot {
        epoch: 1,
        owners: [
            ("a".into(), f(ResourcePhase::Ready, 60)),
            ("b".into(), f(ResourcePhase::Parked, 10)),
        ]
        .into(),
    };
    let obs = [MemoryObservation {
        domain: "system".into(),
        capacity_bytes: 128,
        available_bytes: 58,
        sampled_at_ms: 100,
    }];
    let limits = [MemoryLimit {
        domain: "system".into(),
        managed_bytes: 96,
        free_reserve_bytes: 12,
        host_kv_bytes: None,
        parked_bytes: None,
    }];
    let wake = ForecastStep {
        owner: "b".into(),
        footprint: f(ResourcePhase::Wake, 80),
    };
    let floors = [("a", 60), ("b", 10)].map(|(owner, bytes)| ResidentFloor {
        owner: owner.into(),
        domain: "system".into(),
        bytes,
        sampled_at_ms: 100,
    });
    let context = AdmissionContext::new(&obs, &limits, 101, 60, 4).with_resident_floors(&floors);
    assert_eq!(
        forecast_sequence(&initial, std::slice::from_ref(&wake), context)
            .unwrap_err()
            .reason,
        ResourceError::Insufficient
    );
    let steps = [
        ForecastStep {
            owner: "a".into(),
            footprint: f(ResourcePhase::Parking, 65),
        },
        ForecastStep {
            owner: "a".into(),
            footprint: f(ResourcePhase::Parked, 8),
        },
        wake,
        ForecastStep {
            owner: "b".into(),
            footprint: f(ResourcePhase::Ready, 60),
        },
    ];
    let result = forecast_sequence(&initial, &steps, context).unwrap();
    assert_eq!(result.owners["a"].phase, ResourcePhase::Parked);
    assert_eq!(result.owners["b"].phase, ResourcePhase::Ready);
    assert_eq!(initial.epoch, 1); // Forecast cannot mutate the source ledger.
    let mut impossible = steps.clone();
    impossible[0].footprint = f(ResourcePhase::Parking, 100);
    assert_eq!(
        forecast_sequence(&initial, &impossible, context)
            .unwrap_err()
            .step,
        0
    ); // A parking peak itself can be infeasible.
}

#[test]
fn both_parked_is_not_enough_without_each_wake_path() {
    let initial = LedgerSnapshot {
        epoch: 9,
        owners: [
            ("a".into(), f(ResourcePhase::Parked, 8)),
            ("b".into(), f(ResourcePhase::Parked, 10)),
        ]
        .into(),
    };
    let obs = [MemoryObservation {
        domain: "system".into(),
        capacity_bytes: 128,
        available_bytes: 110,
        sampled_at_ms: 100,
    }];
    let limits = [MemoryLimit {
        domain: "system".into(),
        managed_bytes: 96,
        free_reserve_bytes: 12,
        host_kv_bytes: None,
        parked_bytes: None,
    }];
    for (owner, peak, expected) in [("a", 70, true), ("b", 95, false)] {
        let steps = [ForecastStep {
            owner: owner.into(),
            footprint: f(ResourcePhase::Wake, peak),
        }];
        assert_eq!(
            forecast_sequence(
                &initial,
                &steps,
                AdmissionContext::new(&obs, &limits, 101, 60, 4)
            )
            .is_ok(),
            expected
        );
    }
}

#[test]
fn cleanup_credits_only_qualified_floors_and_preserves_inputs() {
    let initial = LedgerSnapshot {
        epoch: 4,
        owners: [("a".into(), f(ResourcePhase::Ready, 40))].into(),
    };
    let original = initial.clone();
    let obs = [MemoryObservation {
        domain: "system".into(),
        capacity_bytes: 100,
        available_bytes: 60,
        sampled_at_ms: 100,
    }];
    let limits = [MemoryLimit {
        domain: "system".into(),
        managed_bytes: 100,
        free_reserve_bytes: 0,
        host_kv_bytes: None,
        parked_bytes: None,
    }];
    let floors = [ResidentFloor {
        owner: "a".into(),
        domain: "system".into(),
        bytes: 40,
        sampled_at_ms: 100,
    }];
    let actions = [
        ForecastAction::RemoveAfterVerifiedCleanup { owner: "a".into() },
        ForecastAction::Phase(ForecastStep {
            owner: "b".into(),
            footprint: f(ResourcePhase::Cold, 70),
        }),
    ];
    let context = AdmissionContext::new(&obs, &limits, 100, 10, 10);
    let result =
        forecast_actions(&initial, &actions, context.with_resident_floors(&floors)).unwrap();
    assert!(!result.owners.contains_key("a"));
    assert_eq!(result.owners["b"], f(ResourcePhase::Cold, 70));
    assert_eq!(
        forecast_actions(&initial, &actions, context).unwrap_err(),
        SequenceFailure {
            step: 1,
            reason: ResourceError::Insufficient
        }
    );
    assert_eq!(
        forecast_actions(&initial, &actions[..1], context)
            .unwrap()
            .owners
            .len(),
        0
    );
    assert_eq!(initial, original);
    assert_eq!(obs[0].available_bytes, 60);
    assert_eq!(floors[0].bytes, 40);
    assert_eq!(
        forecast_actions(
            &initial,
            &[ForecastAction::RemoveAfterVerifiedCleanup {
                owner: "missing".into()
            }],
            context
        )
        .unwrap_err()
        .reason,
        ResourceError::Invalid
    );
}

#[test]
fn cleanup_rejects_impossible_attribution_before_removal() {
    let initial = LedgerSnapshot {
        epoch: 4,
        owners: [("a".into(), f(ResourcePhase::Ready, 40))].into(),
    };
    let obs = [MemoryObservation {
        domain: "system".into(),
        capacity_bytes: 100,
        available_bytes: 70,
        sampled_at_ms: 100,
    }];
    let limits = [MemoryLimit {
        domain: "system".into(),
        managed_bytes: 100,
        free_reserve_bytes: 0,
        host_kv_bytes: None,
        parked_bytes: None,
    }];
    let floors = [ResidentFloor {
        owner: "a".into(),
        domain: "system".into(),
        bytes: 40,
        sampled_at_ms: 100,
    }];
    let context = AdmissionContext::new(&obs, &limits, 100, 10, 10).with_resident_floors(&floors);
    assert_eq!(
        forecast_actions(
            &initial,
            &[ForecastAction::RemoveAfterVerifiedCleanup { owner: "a".into() }],
            context
        )
        .unwrap_err()
        .reason,
        ResourceError::Invalid
    );
    assert_eq!(
        forecast_sequence(
            &initial,
            &[ForecastStep {
                owner: "a".into(),
                footprint: f(ResourcePhase::Ready, 40)
            }],
            context
        )
        .unwrap_err()
        .reason,
        ResourceError::Invalid
    );
}

#[test]
fn cleanup_after_phase_uses_forecast_floor_and_phase_wrapper_is_equivalent() {
    let initial = LedgerSnapshot {
        epoch: 4,
        owners: [
            ("a".into(), f(ResourcePhase::Ready, 40)),
            ("c".into(), f(ResourcePhase::Ready, 40)),
        ]
        .into(),
    };
    let obs = [MemoryObservation {
        domain: "system".into(),
        capacity_bytes: 100,
        available_bytes: 20,
        sampled_at_ms: 100,
    }];
    let limits = [MemoryLimit {
        domain: "system".into(),
        managed_bytes: 100,
        free_reserve_bytes: 0,
        host_kv_bytes: None,
        parked_bytes: None,
    }];
    let floors = ["a", "c"].map(|owner| ResidentFloor {
        owner: owner.into(),
        domain: "system".into(),
        bytes: 40,
        sampled_at_ms: 100,
    });
    let context = AdmissionContext::new(&obs, &limits, 100, 10, 10).with_resident_floors(&floors);
    let steps = [
        ForecastStep {
            owner: "a".into(),
            footprint: f(ResourcePhase::Parking, 50),
        },
        ForecastStep {
            owner: "a".into(),
            footprint: f(ResourcePhase::Parked, 10),
        },
    ];
    let mut actions: Vec<_> = steps.iter().cloned().map(ForecastAction::Phase).collect();
    assert_eq!(
        forecast_sequence(&initial, &steps, context),
        forecast_actions(&initial, &actions, context)
    );
    actions.push(ForecastAction::RemoveAfterVerifiedCleanup { owner: "a".into() });
    actions.push(ForecastAction::Phase(ForecastStep {
        owner: "b".into(),
        footprint: f(ResourcePhase::Cold, 60),
    }));
    let result = forecast_actions(&initial, &actions, context).unwrap();
    assert!(!result.owners.contains_key("a"));
    assert_eq!(result.owners["c"], initial.owners["c"]);
    assert_eq!(result.owners["b"], f(ResourcePhase::Cold, 60));
    let denied = [ForecastStep {
        owner: "b".into(),
        footprint: f(ResourcePhase::Cold, 60),
    }];
    assert_eq!(
        forecast_sequence(&initial, &denied, context),
        forecast_actions(&initial, &denied.map(ForecastAction::Phase), context)
    );
}

#[test]
fn aggregate_floor_validation_preserves_phase_error_precedence() {
    let mut initial = LedgerSnapshot {
        epoch: 4,
        owners: [
            ("a".into(), f(ResourcePhase::Ready, 40)),
            ("c".into(), f(ResourcePhase::Ready, 40)),
        ]
        .into(),
    };
    let obs = [MemoryObservation {
        domain: "system".into(),
        capacity_bytes: 100,
        available_bytes: 40,
        sampled_at_ms: 100,
    }];
    let limits = [MemoryLimit {
        domain: "system".into(),
        managed_bytes: 100,
        free_reserve_bytes: 0,
        host_kv_bytes: None,
        parked_bytes: None,
    }];
    let floors = ["a", "c"].map(|owner| ResidentFloor {
        owner: owner.into(),
        domain: "system".into(),
        bytes: 40,
        sampled_at_ms: 100,
    });
    let context = AdmissionContext::new(&obs, &limits, 100, 10, 10).with_resident_floors(&floors);
    let cleanup = [ForecastAction::RemoveAfterVerifiedCleanup { owner: "a".into() }];
    assert_eq!(
        forecast_actions(&initial, &cleanup, context)
            .unwrap_err()
            .reason,
        ResourceError::Invalid
    );
    let mut next = f(ResourcePhase::Ready, 1);
    let claims = vec![DeviceClaim {
        device: "gpu".into(),
        sharing: Sharing::Exclusive,
    }];
    initial.owners.get_mut("c").unwrap().devices = claims.clone();
    next.devices = claims;
    assert_eq!(
        capyctl_scheduler::residency::admit_phase(&initial, "b", &next, context),
        Err(ResourceError::DeviceConflict)
    );
    assert_eq!(
        capyctl_scheduler::residency::admit_phase(
            &initial,
            "b",
            &f(ResourcePhase::Parked, 1),
            AdmissionContext {
                max_parked: 0,
                ..context
            }
        ),
        Err(ResourceError::CategoryLimit)
    );
    assert_eq!(
        capyctl_scheduler::residency::admit_phase(&initial, "b", &f(ResourcePhase::Ready, 1), context),
        Err(ResourceError::Invalid)
    );
}
