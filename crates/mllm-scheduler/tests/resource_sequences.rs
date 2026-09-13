use mllm_domain::resources::*;
use mllm_scheduler::{
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
