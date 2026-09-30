use capyctl_controller::sequence::*;
use capyctl_domain::resources::*;
use capyctl_scheduler::{residency::AdmissionContext, sequence::forecast_actions};
use std::collections::BTreeMap;

fn footprint(phase: ResourcePhase, bytes: i64) -> PhaseFootprint {
    PhaseFootprint {
        phase,
        allocations: vec![Allocation {
            domain: "ram".into(),
            bytes,
            host_kv_bytes: 0,
        }],
        devices: vec![],
    }
}

fn spec(cold: i64) -> OwnerPlanSpec {
    OwnerPlanSpec {
        recipe: RecipeFootprints {
            cold: footprint(ResourcePhase::Cold, cold),
            ready: footprint(ResourcePhase::Ready, 40),
            parking: footprint(ResourcePhase::Parking, 50),
            parked: footprint(ResourcePhase::Parked, 10),
            wake: footprint(ResourcePhase::Wake, 60),
        },
        warm: true,
        managed: true,
        qualified_initialize: true,
        qualified_park: true,
        qualified_restore: true,
        opposing_claim: false,
        automatic_reclamation: true,
        admission_window_until_ms: None,
        allow_cold_stop: false,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Fixture {
    initial: LedgerSnapshot,
    owners: BTreeMap<String, OwnerPlanSpec>,
    observations: Vec<MemoryObservation>,
    floors: Vec<ResidentFloor>,
    limits: Vec<MemoryLimit>,
    budget: usize,
}

impl Fixture {
    fn empty() -> Self {
        Self {
            initial: LedgerSnapshot {
                epoch: 17,
                owners: BTreeMap::new(),
            },
            owners: [("A".into(), spec(50)), ("B".into(), spec(70))].into(),
            observations: vec![MemoryObservation {
                domain: "ram".into(),
                capacity_bytes: 100,
                available_bytes: 100,
                sampled_at_ms: 100,
            }],
            floors: vec![],
            limits: vec![MemoryLimit {
                domain: "ram".into(),
                managed_bytes: 100,
                free_reserve_bytes: 0,
                host_kv_bytes: None,
                parked_bytes: None,
            }],
            budget: 100,
        }
    }
    fn ready_a() -> Self {
        let mut f = Self::empty();
        f.retain("A", footprint(ResourcePhase::Ready, 40));
        f
    }
    fn retain(&mut self, owner: &str, footprint: PhaseFootprint) {
        let bytes = footprint.allocations[0].bytes;
        self.initial.owners.insert(owner.into(), footprint);
        self.floors.push(ResidentFloor {
            owner: owner.into(),
            domain: "ram".into(),
            bytes,
            sampled_at_ms: 100,
        });
        self.observations[0].available_bytes -= bytes;
    }
    fn input(&self) -> PlannerInput<'_> {
        PlannerInput {
            initial: &self.initial,
            owners: &self.owners,
            admission: AdmissionContext::new(&self.observations, &self.limits, 100, 10, 10)
                .with_resident_floors(&self.floors),
            max_expansions: self.budget,
        }
    }
}

fn phases(actions: &[ForecastAction]) -> Vec<(&str, ResourcePhase)> {
    actions
        .iter()
        .map(|a| match a {
            ForecastAction::Phase(s) => (s.owner.as_str(), s.footprint.phase),
            ForecastAction::RemoveAfterVerifiedCleanup { .. } => panic!("unexpected cleanup"),
        })
        .collect()
}

fn members(final_state: FinalState) -> Vec<PreparationMember> {
    ["A", "B"]
        .map(|owner| PreparationMember {
            owner: owner.into(),
            final_state,
        })
        .to_vec()
}

fn diagnostic(result: Result<Vec<ForecastAction>, PlanError>) -> PlanDiagnostic {
    match result {
        Err(PlanError::NoSafeSequence(d)) => d,
        other => panic!("expected diagnostic, got {other:?}"),
    }
}

#[test]
fn activation_parks_only_when_direct_peak_cannot_fit() {
    use ResourcePhase::*;
    let mut f = Fixture::ready_a();
    let before = f.clone();
    let actions = plan_activation(f.input(), "B").unwrap();
    assert_eq!(
        phases(&actions),
        [("A", Parking), ("A", Parked), ("B", Cold), ("B", Ready)]
    );
    assert_eq!(
        forecast_actions(&f.initial, &actions, f.input().admission)
            .unwrap()
            .owners["A"],
        footprint(Parked, 10)
    );
    assert_eq!(f, before);
    f.owners.get_mut("B").unwrap().recipe.cold = footprint(Cold, 50);
    assert_eq!(
        phases(&plan_activation(f.input(), "B").unwrap()),
        [("B", Cold), ("B", Ready)]
    );
}

#[test]
fn activation_accounts_for_parking_peak_observed_increase_and_immutable_owners() {
    for case in 0..3 {
        let mut f = Fixture::ready_a();
        match case {
            0 => {
                f.owners.get_mut("A").unwrap().recipe.parking =
                    footprint(ResourcePhase::Parking, 110)
            }
            1 => {
                f.observations[0].available_bytes = 5;
                f.floors.clear();
            }
            _ => f.retain("C", footprint(ResourcePhase::Ready, 31)),
        }
        let before = f.clone();
        let d = diagnostic(plan_activation(f.input(), "B"));
        assert_eq!(d.target, "B");
        assert_eq!(d.reason, Some(ResourceError::Insufficient));
        assert_eq!(
            phases(&d.attempted_prefix),
            [("B", ResourcePhase::Cold), ("B", ResourcePhase::Ready)]
        );
        assert_eq!(d.failed_step, d.attempted_prefix.first().cloned());
        assert_eq!(d.observations, f.observations);
        assert_eq!(d.limits, f.limits);
        assert_eq!(f, before);
    }
}

#[test]
fn activation_reclaims_multiple_owners_in_id_order_and_keeps_identity() {
    let mut f = Fixture::ready_a();
    let mut c = spec(40);
    c.recipe.ready = footprint(ResourcePhase::Ready, 30);
    c.recipe.parking = footprint(ResourcePhase::Parking, 35);
    f.owners.insert("C".into(), c);
    f.retain("C", footprint(ResourcePhase::Ready, 30));
    assert_eq!(
        phases(&plan_activation(f.input(), "B").unwrap()),
        [
            ("A", ResourcePhase::Parking),
            ("A", ResourcePhase::Parked),
            ("C", ResourcePhase::Parking),
            ("C", ResourcePhase::Parked),
            ("B", ResourcePhase::Cold),
            ("B", ResourcePhase::Ready)
        ]
    );
    f.owners.get_mut("C").unwrap().recipe.ready = footprint(ResourcePhase::Ready, 40);
    f.owners.get_mut("C").unwrap().recipe.parking = footprint(ResourcePhase::Parking, 50);
    f.initial
        .owners
        .insert("C".into(), footprint(ResourcePhase::Ready, 40));
    f.floors[1].bytes = 40;
    f.observations[0].available_bytes = 20;
    assert_eq!(phases(&plan_activation(f.input(), "B").unwrap())[0].0, "A");
}

#[test]
fn node_budget_distinguishes_pruning_from_denial_and_still_allows_direct_success() {
    let mut f = Fixture::ready_a();
    f.budget = 1;
    assert_eq!(
        plan_activation(f.input(), "B"),
        Err(PlanError::SearchLimit {
            expanded: 1,
            limit: 1
        })
    );
    f.owners.get_mut("B").unwrap().recipe.cold = footprint(ResourcePhase::Cold, 50);
    assert!(plan_activation(f.input(), "B").is_ok());
    f.owners.get_mut("B").unwrap().recipe.cold = footprint(ResourcePhase::Cold, 70);
    f.owners.get_mut("A").unwrap().recipe.parking = footprint(ResourcePhase::Parking, 110);
    diagnostic(plan_activation(f.input(), "B"));
}

#[test]
fn victim_gates_do_not_change_input_or_windows() {
    type Gate = fn(&mut OwnerPlanSpec);
    let gates: [(Gate, Exclusion); 5] = [
        (|s| s.managed = false, Exclusion::Attached),
        (|s| s.opposing_claim = true, Exclusion::OpposingClaim),
        (
            |s| s.automatic_reclamation = false,
            Exclusion::AutomaticReclamationDisabled,
        ),
        (
            |s| s.admission_window_until_ms = Some(101),
            Exclusion::AdmissionWindowOpen,
        ),
        (|s| s.qualified_park = false, Exclusion::UnqualifiedPark),
    ];
    for (gate, exclusion) in gates {
        let mut f = Fixture::ready_a();
        gate(f.owners.get_mut("A").unwrap());
        let before = f.clone();
        assert_eq!(
            diagnostic(plan_activation(f.input(), "B"))
                .excluded
                .get("A"),
            Some(&exclusion)
        );
        assert_eq!(f, before);
    }
    let mut f = Fixture::ready_a();
    f.owners.get_mut("A").unwrap().admission_window_until_ms = Some(100);
    assert!(plan_activation(f.input(), "B").is_ok());
}

#[test]
fn restart_only_cleanup_is_explicit_and_never_granted_by_warm_intent() {
    let mut f = Fixture::ready_a();
    let a = f.owners.get_mut("A").unwrap();
    a.qualified_park = false;
    a.allow_cold_stop = true;
    assert_eq!(
        diagnostic(plan_activation(f.input(), "B")).excluded["A"],
        Exclusion::UnqualifiedPark
    );
    f.owners.get_mut("A").unwrap().warm = false;
    let actions = plan_activation(f.input(), "B").unwrap();
    assert_eq!(
        actions[0],
        ForecastAction::RemoveAfterVerifiedCleanup { owner: "A".into() }
    );
    assert_eq!(
        phases(&actions[1..]),
        [("B", ResourcePhase::Cold), ("B", ResourcePhase::Ready)]
    );
    f.floors.clear();
    diagnostic(plan_activation(f.input(), "B"));
}

#[test]
fn preparation_initializes_in_order_then_finds_final_ready_arrangement() {
    use ResourcePhase::*;
    let f = Fixture::empty();
    let before = f.clone();
    let actions = plan_preparation(f.input(), &members(FinalState::Ready)).unwrap();
    assert_eq!(
        phases(&actions),
        [
            ("A", Cold),
            ("A", Ready),
            ("A", Parking),
            ("A", Parked),
            ("B", Cold),
            ("B", Ready),
            ("A", Wake),
            ("A", Ready)
        ]
    );
    assert_eq!(f, before);
}

#[test]
fn preparation_all_parked_requires_independent_future_wakes() {
    let mut f = Fixture::empty();
    let actions = plan_preparation(f.input(), &members(FinalState::Parked)).unwrap();
    assert!(!phases(&actions)
        .iter()
        .any(|(_, phase)| *phase == ResourcePhase::Wake));
    let result = forecast_actions(&f.initial, &actions, f.input().admission).unwrap();
    assert!(result
        .owners
        .values()
        .all(|v| v.phase == ResourcePhase::Parked));
    f.owners.get_mut("A").unwrap().recipe.wake = footprint(ResourcePhase::Wake, 95);
    let d = diagnostic(plan_preparation(f.input(), &members(FinalState::Parked)));
    assert_eq!(d.target, "A");
    assert_eq!(d.reason, Some(ResourceError::Insufficient));
    assert_eq!(
        d.failed_step
            .as_ref()
            .map(|a| phases(std::slice::from_ref(a))[0].1),
        Some(ResourcePhase::Wake)
    );
}

#[test]
fn preparation_preserves_outsiders_and_retained_parked_members() {
    let mut f = Fixture::empty();
    f.retain("C", footprint(ResourcePhase::Ready, 31));
    diagnostic(plan_preparation(f.input(), &members(FinalState::Parked)));
    let mut f = Fixture::empty();
    f.retain("A", footprint(ResourcePhase::Parked, 10));
    let actions = plan_preparation(f.input(), &members(FinalState::Parked)).unwrap();
    assert!(phases(&actions).iter().all(|(id, _)| *id != "A"));
    let actions = plan_preparation(f.input(), &members(FinalState::Ready)).unwrap();
    assert_eq!(
        phases(&actions),
        [
            ("B", ResourcePhase::Cold),
            ("B", ResourcePhase::Ready),
            ("A", ResourcePhase::Wake),
            ("A", ResourcePhase::Ready)
        ]
    );
}

#[test]
fn only_requested_final_park_bypasses_window_and_automatic_gates() {
    for window in [false, true] {
        let mut f = Fixture::ready_a();
        let a = f.owners.get_mut("A").unwrap();
        if window {
            a.admission_window_until_ms = Some(101);
        } else {
            a.automatic_reclamation = false;
        }
        assert!(plan_preparation(f.input(), &members(FinalState::Parked)).is_ok());
        let d = diagnostic(plan_preparation(f.input(), &members(FinalState::Ready)));
        assert_eq!(
            d.excluded["A"],
            if window {
                Exclusion::AdmissionWindowOpen
            } else {
                Exclusion::AutomaticReclamationDisabled
            }
        );
    }
}

#[test]
fn invalid_contracts_and_requested_owner_gates_fail_before_search() {
    let mut f = Fixture::ready_a();
    f.owners.get_mut("A").unwrap().recipe.ready = footprint(ResourcePhase::Ready, 41);
    assert_eq!(plan_activation(f.input(), "B"), Err(PlanError::Invalid));
    let f = Fixture::empty();
    assert_eq!(plan_activation(f.input(), ""), Err(PlanError::Invalid));
    assert_eq!(
        plan_activation(f.input(), "missing"),
        Err(PlanError::Invalid)
    );
    assert_eq!(
        plan_preparation(f.input(), &vec![members(FinalState::Ready)[0].clone(); 2]),
        Err(PlanError::Invalid)
    );
    for exclusion in [
        Exclusion::Attached,
        Exclusion::OpposingClaim,
        Exclusion::UnqualifiedInitialize,
        Exclusion::UnqualifiedPark,
        Exclusion::UnqualifiedRestore,
    ] {
        let mut f = Fixture::empty();
        let a = f.owners.get_mut("A").unwrap();
        match exclusion {
            Exclusion::Attached => a.managed = false,
            Exclusion::OpposingClaim => a.opposing_claim = true,
            Exclusion::UnqualifiedInitialize => a.qualified_initialize = false,
            Exclusion::UnqualifiedPark => a.qualified_park = false,
            Exclusion::UnqualifiedRestore => a.qualified_restore = false,
            _ => unreachable!(),
        }
        assert_eq!(
            diagnostic(plan_preparation(f.input(), &members(FinalState::Parked))).excluded["A"],
            exclusion
        );
    }
}

#[test]
fn warm_commitment_never_grants_cold_eviction() {
    assert!(!may_reclaim(true, false, true));
    assert!(may_reclaim(true, true, false));
    assert!(may_reclaim(false, false, true));
    assert!(!may_reclaim(false, false, false));
}

#[test]
fn preparation_requires_warm_intent_even_when_park_is_qualified() {
    let mut f = Fixture::empty();
    f.owners.get_mut("A").unwrap().warm = false;
    let d = diagnostic(plan_preparation(f.input(), &members(FinalState::Parked)));
    assert_eq!(d.excluded["A"], Exclusion::WarmResidencyRequired);
    assert_eq!(d.reason, None);
    assert_eq!(d.failed_step, None);
    assert!(d.attempted_prefix.is_empty());
}

#[test]
fn cleanup_remains_an_alternative_after_qualified_park_and_for_retained_parked() {
    let mut f = Fixture::ready_a();
    f.owners.get_mut("B").unwrap().recipe.cold = footprint(ResourcePhase::Cold, 95);
    let a = f.owners.get_mut("A").unwrap();
    a.warm = false;
    a.allow_cold_stop = true;
    let actions = plan_activation(f.input(), "B").unwrap();
    assert_eq!(
        actions[0],
        ForecastAction::RemoveAfterVerifiedCleanup { owner: "A".into() }
    );
    let mut f = Fixture::empty();
    f.retain("A", footprint(ResourcePhase::Parked, 10));
    f.owners.get_mut("A").unwrap().warm = false;
    f.owners.get_mut("A").unwrap().allow_cold_stop = true;
    f.owners.get_mut("B").unwrap().recipe.cold = footprint(ResourcePhase::Cold, 95);
    assert_eq!(
        plan_activation(f.input(), "B").unwrap()[0],
        ForecastAction::RemoveAfterVerifiedCleanup { owner: "A".into() }
    );
    let mut f = Fixture::ready_a();
    f.owners.get_mut("A").unwrap().warm = false;
    f.owners.get_mut("A").unwrap().allow_cold_stop = true;
    assert_eq!(
        phases(&plan_activation(f.input(), "B").unwrap())[0],
        ("A", ResourcePhase::Parking)
    );
}

#[test]
fn requested_activation_gates_and_transient_owners_remain_conservative() {
    let mut f = Fixture::ready_a();
    assert_eq!(plan_activation(f.input(), "A").unwrap(), vec![]);
    f.owners.get_mut("A").unwrap().opposing_claim = true;
    assert_eq!(
        diagnostic(plan_activation(f.input(), "A")).excluded["A"],
        Exclusion::OpposingClaim
    );
    let mut f = Fixture::empty();
    f.retain("A", footprint(ResourcePhase::Parked, 10));
    assert_eq!(
        phases(&plan_activation(f.input(), "A").unwrap()),
        [("A", ResourcePhase::Wake), ("A", ResourcePhase::Ready)]
    );
    f.owners.get_mut("A").unwrap().qualified_restore = false;
    assert_eq!(
        diagnostic(plan_activation(f.input(), "A")).excluded["A"],
        Exclusion::UnqualifiedRestore
    );
    let mut f = Fixture::empty();
    f.owners.get_mut("B").unwrap().qualified_initialize = false;
    assert_eq!(
        diagnostic(plan_activation(f.input(), "B")).excluded["B"],
        Exclusion::UnqualifiedInitialize
    );
    for phase in [
        ResourcePhase::Cold,
        ResourcePhase::Parking,
        ResourcePhase::Wake,
    ] {
        let mut f = Fixture::empty();
        f.retain("A", footprint(phase, 50));
        let before = f.clone();
        assert_eq!(
            diagnostic(plan_activation(f.input(), "A")).excluded["A"],
            Exclusion::TransientPhase
        );
        assert_eq!(
            diagnostic(plan_activation(f.input(), "B")).excluded["A"],
            Exclusion::TransientPhase
        );
        assert_eq!(f, before);
    }
}

#[test]
fn diagnostic_preserves_missing_stale_device_and_category_denials() {
    for case in 0..5 {
        let mut f = Fixture::ready_a();
        f.owners.get_mut("A").unwrap().automatic_reclamation = false;
        f.owners.get_mut("B").unwrap().recipe.cold = footprint(ResourcePhase::Cold, 50);
        let expected = match case {
            0 => {
                f.observations.clear();
                f.floors.clear();
                ResourceError::UnknownDomain
            }
            1 => {
                f.observations[0].sampled_at_ms = 89;
                f.floors[0].sampled_at_ms = 89;
                ResourceError::StaleObservation
            }
            2 => {
                let claims = vec![DeviceClaim {
                    device: "gpu0".into(),
                    sharing: Sharing::Exclusive,
                }];
                f.initial.owners.get_mut("A").unwrap().devices = claims.clone();
                f.owners.get_mut("A").unwrap().recipe.ready.devices = claims.clone();
                f.owners.get_mut("B").unwrap().recipe.cold.devices = claims;
                ResourceError::DeviceConflict
            }
            3 => {
                f.limits[0].host_kv_bytes = Some(5);
                f.owners.get_mut("B").unwrap().recipe.cold.allocations[0].host_kv_bytes = 6;
                ResourceError::CategoryLimit
            }
            _ => {
                f.limits[0].parked_bytes = Some(5);
                f.owners.get_mut("B").unwrap().recipe.cold = footprint(ResourcePhase::Cold, 70);
                f.owners.get_mut("A").unwrap().automatic_reclamation = true;
                ResourceError::Insufficient // First direct denial is preserved; park fails the category cap.
            }
        };
        let before = f.clone();
        assert_eq!(
            diagnostic(plan_activation(f.input(), "B")).reason,
            Some(expected)
        );
        assert_eq!(f, before);
    }
    let mut f = Fixture::ready_a();
    f.limits[0].parked_bytes = Some(5);
    let request = [PreparationMember {
        owner: "A".into(),
        final_state: FinalState::Parked,
    }];
    assert_eq!(
        diagnostic(plan_preparation(f.input(), &request)).reason,
        Some(ResourceError::CategoryLimit)
    );
    let mut f = Fixture::ready_a();
    f.observations[0].available_bytes = 70;
    assert_eq!(plan_activation(f.input(), "B"), Err(PlanError::Invalid));
}

#[test]
fn validation_rejects_all_malformed_entries_and_bounds() {
    for case in 0..7 {
        let mut f = Fixture::empty();
        match case {
            0 => f.budget = 0,
            1 => f.owners.get_mut("A").unwrap().admission_window_until_ms = Some(-1),
            2 => f.owners.get_mut("A").unwrap().recipe.cold.allocations[0].bytes = 1,
            3 => {
                f.initial
                    .owners
                    .insert("outsider".into(), footprint(ResourcePhase::Ready, -1));
            }
            4 => {
                f.initial
                    .owners
                    .insert("".into(), footprint(ResourcePhase::Ready, 0));
            }
            5 => {
                f.owners.insert("".into(), spec(50));
            }
            _ => {
                for i in 0..1023 {
                    f.owners.insert(format!("extra-{i}"), spec(50));
                }
            }
        }
        assert_eq!(
            plan_activation(f.input(), "B"),
            Err(PlanError::Invalid),
            "case {case}"
        );
    }
    let f = Fixture::empty();
    assert_eq!(plan_preparation(f.input(), &[]), Err(PlanError::Invalid));
    assert_eq!(
        plan_preparation(
            f.input(),
            &vec![
                PreparationMember {
                    owner: "A".into(),
                    final_state: FinalState::Ready
                };
                1025
            ]
        ),
        Err(PlanError::Invalid)
    );
}

#[test]
fn final_wakes_can_reorder_against_initialization_order() {
    // A's wake fits only while B is parked; B can wake after A settles.
    let mut f = Fixture::empty();
    f.retain("A", footprint(ResourcePhase::Parked, 10));
    f.retain("B", footprint(ResourcePhase::Parked, 10));
    f.owners.get_mut("A").unwrap().recipe.wake = footprint(ResourcePhase::Wake, 85);
    let mut request = members(FinalState::Ready);
    request.reverse();
    assert_eq!(
        phases(&plan_preparation(f.input(), &request).unwrap()),
        [
            ("A", ResourcePhase::Wake),
            ("A", ResourcePhase::Ready),
            ("B", ResourcePhase::Wake),
            ("B", ResourcePhase::Ready)
        ]
    );
}

#[test]
fn queued_success_survives_budget_pruning_of_later_valid_nodes() {
    let mut f = Fixture::ready_a();
    let mut c = spec(40);
    c.recipe.ready = footprint(ResourcePhase::Ready, 30);
    c.recipe.parking = footprint(ResourcePhase::Parking, 35);
    f.owners.insert("C".into(), c);
    f.retain("C", footprint(ResourcePhase::Ready, 30));
    f.owners.get_mut("B").unwrap().recipe.cold = footprint(ResourcePhase::Cold, 60);
    f.budget = 2;
    assert_eq!(phases(&plan_activation(f.input(), "B").unwrap())[0].0, "A");
    f.owners.get_mut("B").unwrap().recipe.cold = footprint(ResourcePhase::Cold, 70);
    f.budget = 3;
    assert_eq!(
        plan_activation(f.input(), "B"),
        Err(PlanError::SearchLimit {
            expanded: 3,
            limit: 3
        })
    );
}

#[test]
fn future_wake_probes_do_not_accumulate_and_noop_still_validates_evidence() {
    let mut f = Fixture::empty();
    f.owners.get_mut("A").unwrap().recipe.wake = footprint(ResourcePhase::Wake, 80);
    f.owners.get_mut("B").unwrap().recipe.wake = footprint(ResourcePhase::Wake, 80);
    let actions = plan_preparation(f.input(), &members(FinalState::Parked)).unwrap();
    assert!(!phases(&actions)
        .iter()
        .any(|(_, p)| *p == ResourcePhase::Wake));
    let mut f = Fixture::empty();
    f.retain("A", footprint(ResourcePhase::Parked, 10));
    let request = [PreparationMember {
        owner: "A".into(),
        final_state: FinalState::Parked,
    }];
    assert!(plan_preparation(f.input(), &request).unwrap().is_empty());
    f.observations[0].sampled_at_ms = 89;
    assert_eq!(
        diagnostic(plan_preparation(f.input(), &request)).reason,
        Some(ResourceError::StaleObservation)
    );
    let mut f = Fixture::ready_a();
    f.observations[0].sampled_at_ms = 89;
    assert_eq!(
        diagnostic(plan_activation(f.input(), "A")).reason,
        Some(ResourceError::StaleObservation)
    );
}

#[test]
fn preparation_never_reclaims_unvisited_retained_members() {
    let mut f = Fixture::empty();
    f.retain("B", footprint(ResourcePhase::Ready, 40));
    f.owners.get_mut("A").unwrap().recipe.cold = footprint(ResourcePhase::Cold, 70);
    let d = diagnostic(plan_preparation(f.input(), &members(FinalState::Parked)));
    assert_eq!(
        phases(&d.attempted_prefix),
        [("A", ResourcePhase::Cold), ("A", ResourcePhase::Ready)]
    );
    assert_eq!(d.reason, Some(ResourceError::Insufficient));
}

#[test]
fn retained_recipe_matching_ignores_allocation_and_device_order() {
    let mut f = Fixture::empty();
    let a = f.owners.get_mut("A").unwrap();
    let claims = vec![
        DeviceClaim {
            device: "gpu1".into(),
            sharing: Sharing::Shared,
        },
        DeviceClaim {
            device: "gpu0".into(),
            sharing: Sharing::Exclusive,
        },
    ];
    for phase in [
        &mut a.recipe.cold,
        &mut a.recipe.ready,
        &mut a.recipe.parking,
        &mut a.recipe.parked,
        &mut a.recipe.wake,
    ] {
        phase.allocations.push(Allocation {
            domain: "other".into(),
            bytes: 0,
            host_kv_bytes: 0,
        });
        if phase.phase != ResourcePhase::Parked {
            phase.devices = claims.clone();
        }
    }
    let mut retained = a.recipe.ready.clone();
    retained.allocations.reverse();
    retained.devices.reverse();
    f.initial.owners.insert("A".into(), retained);
    f.observations.push(MemoryObservation {
        domain: "other".into(),
        capacity_bytes: 100,
        available_bytes: 100,
        sampled_at_ms: 100,
    });
    f.limits.push(MemoryLimit {
        domain: "other".into(),
        managed_bytes: 100,
        free_reserve_bytes: 0,
        host_kv_bytes: None,
        parked_bytes: None,
    });
    let before = f.clone();
    assert!(plan_activation(f.input(), "A").unwrap().is_empty());
    assert_eq!(f, before);
    f.initial.owners.get_mut("A").unwrap().devices[0].sharing = Sharing::Shared;
    assert_eq!(plan_activation(f.input(), "A"), Err(PlanError::Invalid));
}

mod host_scoped {
    use super::*;
    use capyctl_config::effective::{
        DevicePolicy, DomainMemory, DomainPolicy, HostPolicy, PortRange, QueuePolicy, Sharing,
    };

    fn host() -> HostPolicy {
        HostPolicy {
            name: "host".into(),
            hardware_fingerprint: "hw".into(),
            environment_fingerprint: "env".into(),
            device_inventory_digest: None,
            model_store: "/srv/models".into(),
            domains: BTreeMap::from([(
                "ram".into(),
                DomainPolicy {
                    managed_limit: 100,
                    free_reserve: 0,
                    host_kv_limit: None,
                    parked_limit: None,
                    memory: DomainMemory::Distinct,
                    device: None,
                },
            )]),
            devices: BTreeMap::from([(
                "gpu0".into(),
                DevicePolicy {
                    physical_gpu_uuid: None,
                    domain: "ram".into(),
                    sharing: Sharing::Shared,
                },
            )]),
            max_parked: 0,
            observation_ttl_ms: 2_000,
            device_sharing: Sharing::Shared,
            endpoint_port_range: PortRange {
                start: 20_000,
                end: 20_100,
            },
            planner_max_states: 4_096,
            queue: QueuePolicy {
                max_pending_per_deployment: 64,
                max_pending_total: 256,
                max_buffered_bytes_total: 64 << 20,
                request_deadline_ms: 600_000,
                admission_window_ms: 2_000,
                stream_idle_ms: capyctl_config::effective::DEFAULT_STREAM_IDLE_MS,
            },
            model_sources: Default::default(),
        }
    }

    fn scoped(owner_domain: &str, bytes: i64, phase: ResourcePhase) -> PhaseFootprint {
        PhaseFootprint {
            phase,
            allocations: vec![Allocation {
                domain: owner_domain.into(),
                bytes,
                host_kv_bytes: 0,
            }],
            devices: vec![],
        }
    }

    // T26 T27 (Phase B follow-up): a park or switch plan on one host is judged
    // against that host's owners. Another host's parked owner neither fails the
    // plan as an unknown domain nor counts against this host's max_parked (0).
    #[test]
    fn a_plan_for_one_host_is_not_refused_over_another_hosts_parked_owner() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ledger.sqlite3");
        let store = capyctl_store::Store::open(&path).unwrap();
        let session = store.begin_coordinator_session().unwrap();
        let observed = vec![MemoryObservation {
            domain: "ram".into(),
            capacity_bytes: 100,
            available_bytes: 100,
            sampled_at_ms: 1_000,
        }];
        for id in ["host-one", "host-two"] {
            store
                .import_remote_resource_policy(&session, id, &host(), &observed, 1_500)
                .unwrap();
        }
        let ours = store
            .host_resource_key("host-one", "domain", "ram")
            .unwrap()
            .unwrap();
        let theirs = store
            .host_resource_key("host-two", "domain", "ram")
            .unwrap()
            .unwrap();
        let parked = serde_json::json!({"version":1,"phase":"parked","allocations":[[theirs,60,0]],"devices":[]});
        let sql = rusqlite::Connection::open(&path).unwrap();
        sql.execute("INSERT INTO deployments(id,name,kind,route_model_id,desired_state,admission_enabled,suspended,current_generation,schema_version) VALUES('X','X','model',NULL,'stopped',1,0,1,1)", []).unwrap();
        sql.execute(
            "INSERT INTO resource_owners(owner_id,footprint_json,deployment_id) VALUES('X',?1,'X')",
            [parked.to_string()],
        )
        .unwrap();

        let recipe = |domain: &str| RecipeFootprints {
            cold: scoped(domain, 50, ResourcePhase::Cold),
            ready: scoped(domain, 40, ResourcePhase::Ready),
            parking: scoped(domain, 50, ResourcePhase::Parking),
            parked: scoped(domain, 10, ResourcePhase::Parked),
            wake: scoped(domain, 60, ResourcePhase::Wake),
        };
        let mut plan_spec = spec(50);
        plan_spec.recipe = recipe(&ours);
        let owners: BTreeMap<String, OwnerPlanSpec> = [("B".to_string(), plan_spec)].into();
        let observations = vec![MemoryObservation {
            domain: ours.clone(),
            capacity_bytes: 100,
            available_bytes: 100,
            sampled_at_ms: 100,
        }];
        let limits = vec![MemoryLimit {
            domain: ours.clone(),
            managed_bytes: 100,
            free_reserve_bytes: 0,
            host_kv_bytes: None,
            parked_bytes: None,
        }];
        let input = |initial| PlannerInput {
            initial,
            owners: &owners,
            admission: AdmissionContext::new(&observations, &limits, 100, 10, 0),
            max_expansions: 100,
        };
        // The whole ledger: the other host's owner is an unknown domain here.
        let whole = store.resource_snapshot().unwrap();
        assert!(plan_activation(input(&whole), "B").is_err());
        // Scoped to this host: the plan proceeds, and max_parked 0 is not
        // exhausted by the other host's parked owner.
        let scoped_ledger = store
            .host_scoped_resource_snapshot(&[ours.as_str()])
            .unwrap();
        assert!(!scoped_ledger.owners.contains_key("X"));
        assert_eq!(scoped_ledger.epoch, whole.epoch);
        assert!(plan_activation(input(&scoped_ledger), "B").is_ok());
    }
}
