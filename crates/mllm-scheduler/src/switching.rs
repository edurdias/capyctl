//! ADR 0013 §8, SPEC §10 (W10): which READY instances one host releases so
//! that an instance of a waiting deployment fits there.
//!
//! Pure and deterministic. The caller supplies the ledger as the chosen host's
//! admission sees it (every owner charged on it, starts accepted but not yet
//! armed included, reclaimable parked owners already removed), the waiting
//! instance's activation footprint on that host, and the READY instances that
//! may be evicted. Nothing here releases or reserves anything: each victim
//! leaves the ledger only on its own verified park or cleanup, and the waiting
//! instance is judged again, with fresh observations and an epoch
//! compare-and-swap, when it arms (ADR 0007).

use std::cmp::Ordering;

use mllm_domain::resources::{LedgerSnapshot, MemoryLimit, PhaseFootprint};

use crate::placement::{fits, HostRefusal};

/// One READY instance on the host that may be released for the waiting one.
///
/// Instances serving a waiting group or holding a warm-residency commitment
/// (SPEC §6.5) are never offered by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VictimCandidate {
    /// The instance's resource owner (`deployment:<id>/instance:<k>`).
    pub owner: String,
    /// ADR 0013 §8 rule 4: its deployment keeps another READY instance that is
    /// not being released, so its route keeps serving.
    pub serves_elsewhere: bool,
    /// When the router last sent it a request, or when it last became READY;
    /// the least recently used goes first among equals.
    pub last_used_ms: i64,
    /// SPEC §6.2, discrete GPU design §5: the footprint it holds once parked
    /// at its declared tier (for `host_backed`, the weights copy on the
    /// system domain), or `None` when it never parks (`restart_only`).
    pub parked: Option<PhaseFootprint>,
}

/// Discrete GPU design §5 ("When a copy does not fit"): how one victim is
/// released.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Release {
    /// Parked at its declared tier: its parked footprint stays charged.
    Park,
    /// Stopped ordinarily: nothing of it stays charged once gone.
    Stop,
}

/// One victim the waiting instance needs released, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Victim {
    pub owner: String,
    pub release: Release,
}

/// ADR 0013 §8 rule 4: instances whose deployment keeps serving elsewhere
/// first, then least recently used, then by owner id so ties are
/// deterministic.
pub fn order_victims(candidates: &mut [VictimCandidate]) {
    candidates.sort_by(|a, b| {
        b.serves_elsewhere
            .cmp(&a.serves_elsewhere)
            .then(a.last_used_ms.cmp(&b.last_used_ms))
            .then_with(|| a.owner.cmp(&b.owner))
            .then(Ordering::Equal)
    });
}

/// ADR 0013 §8 rules 3–4: the victims, in preference order, whose release
/// lets `footprint` fit for `owner` on this host, each with how it is released.
///
/// `Ok(vec![])` means it fits already. `Err` carries the host's reason when
/// even releasing every candidate does not make it fit. The set is minimal:
/// the shortest prefix of `ordered` that fits is taken, then every victim
/// whose release turns out unnecessary once the later ones are released is
/// kept serving, starting from the least preferred.
///
/// Discrete GPU design §5: a victim is released by [`Release::Park`] only when,
/// with every chosen victim applied, the ledger holding its parked footprint
/// still fits the waiting footprint (every domain's managed and parked limits,
/// and `max_parked`). Parking is tried in preference order; a victim whose
/// parked footprint would make the fit fail, or that never parks, is released
/// by [`Release::Stop`]. A copy that does not fit therefore becomes a stop,
/// never an overcommit and never a silent change of tier.
pub fn choose_victims(
    ledger: &LedgerSnapshot,
    owner: &str,
    footprint: &PhaseFootprint,
    limits: &[MemoryLimit],
    max_parked: usize,
    ordered: &[VictimCandidate],
) -> Result<Vec<Victim>, HostRefusal> {
    choose_victims_within(
        ledger,
        owner,
        footprint,
        limits,
        max_parked,
        ordered,
        &|_| true,
    )
}

/// [`choose_victims`], keeping a park only when `room` also accepts the state
/// it leaves (every victim applied, the waiting footprint charged to `owner`).
///
/// Final review I4 (found live on the discrete-GPU laptop host, DG1): the
/// ledger alone does not see memory other programs hold, so a park the
/// ledger admits was refused at arm and its victim stopped anyway. `room` is
/// the fresh observation's verdict ([`observed_room`]); the victim set itself
/// is still chosen from the ledger, so nothing is released on the
/// observation alone.
pub fn choose_victims_within(
    ledger: &LedgerSnapshot,
    owner: &str,
    footprint: &PhaseFootprint,
    limits: &[MemoryLimit],
    max_parked: usize,
    ordered: &[VictimCandidate],
    room: &dyn Fn(&LedgerSnapshot) -> bool,
) -> Result<Vec<Victim>, HostRefusal> {
    let without = |released: &[String]| {
        let mut state = ledger.clone();
        for victim in released {
            state.owners.remove(victim);
        }
        state
    };
    let fits_without =
        |released: &[String]| fits(&without(released), owner, footprint, limits, max_parked);
    let mut chosen: Vec<String> = Vec::new();
    let mut last = fits_without(&chosen);
    for candidate in ordered {
        if last.is_ok() {
            break;
        }
        if candidate.owner == owner || !ledger.owners.contains_key(&candidate.owner) {
            continue;
        }
        chosen.push(candidate.owner.clone());
        last = fits_without(&chosen);
    }
    last?;
    // Keep serving whatever the later victims made unnecessary, least
    // preferred victim first, so the set is minimal (never evict more than
    // the fit needs).
    let mut index = chosen.len();
    while index > 0 {
        index -= 1;
        let mut trial = chosen.clone();
        trial.remove(index);
        if fits_without(&trial).is_ok() {
            chosen = trial;
        }
    }
    let parked_of = |victim: &str| {
        ordered
            .iter()
            .find(|c| c.owner == victim)
            .and_then(|c| c.parked.clone())
    };
    // Every victim is released fully in the set found above, so the all-stop
    // outcome fits. Parking only adds charges back, so each park is kept only
    // when the fit survives it with the later victims still assumed parked;
    // turning a later one into a stop afterwards only frees memory.
    let fits_with = |releases: &[Victim]| {
        let mut state = ledger.clone();
        for victim in releases {
            match (victim.release, parked_of(&victim.owner)) {
                (Release::Park, Some(parked)) => {
                    state.owners.insert(victim.owner.clone(), parked);
                }
                _ => {
                    state.owners.remove(&victim.owner);
                }
            }
        }
        if fits(&state, owner, footprint, limits, max_parked).is_err() {
            return false;
        }
        state.owners.insert(owner.to_owned(), footprint.clone());
        room(&state)
    };
    let mut releases: Vec<Victim> = chosen
        .into_iter()
        .map(|owner| Victim {
            owner,
            release: Release::Park,
        })
        .collect();
    for index in 0..releases.len() {
        if parked_of(&releases[index].owner).is_none() || !fits_with(&releases) {
            releases[index].release = Release::Stop;
        }
    }
    Ok(releases)
}

/// Final review I4 (ADR 0007, SPEC §7): whether `state` (the owners charged
/// after a switch, the waiting owner included) leaves each observed domain its
/// free reserve. The memory each owner of `original` was sampled holding
/// (`floors`, a verified lower bound) returns to the host when it stops or
/// shrinks, and every owner of `state` may use its whole charge:
///
/// `available + sum(floors of original) - sum(charges of state) >= reserve`.
///
/// An owner with no floor frees nothing the planner can prove, so without a
/// process sample the verdict is conservative (a stop, never an overcommit).
/// A domain without an observation is left to the ledger. This is
/// `min(ledger room, observed free memory minus the reserve)`; the planner
/// uses it to decide park or stop, as the arm judges the park afterwards.
pub fn observed_room(
    original: &LedgerSnapshot,
    limits: &[MemoryLimit],
    observations: &[mllm_domain::resources::MemoryObservation],
    floors: &[mllm_domain::resources::ResidentFloor],
    state: &LedgerSnapshot,
) -> bool {
    let amount = |footprint: &PhaseFootprint, domain: &str| {
        footprint
            .allocations
            .iter()
            .filter(|a| a.domain == domain)
            .fold(0_i64, |sum, a| sum.saturating_add(a.bytes))
    };
    limits.iter().all(|limit| {
        let Some(observed) = observations.iter().find(|o| o.domain == limit.domain) else {
            return true;
        };
        let returned = floors
            .iter()
            .filter(|f| f.domain == limit.domain && original.owners.contains_key(&f.owner))
            .fold(0_i64, |sum, f| sum.saturating_add(f.bytes));
        let charged = state.owners.values().fold(0_i64, |sum, footprint| {
            sum.saturating_add(amount(footprint, &limit.domain))
        });
        observed
            .available_bytes
            .saturating_add(returned)
            .saturating_sub(charged)
            >= limit.free_reserve_bytes
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mllm_domain::resources::{Allocation, ResourcePhase};

    const GIB: i64 = 1 << 30;

    fn footprint(bytes: i64, phase: ResourcePhase) -> PhaseFootprint {
        PhaseFootprint {
            phase,
            allocations: vec![Allocation {
                domain: "unified".into(),
                bytes,
                host_kv_bytes: 0,
            }],
            devices: vec![],
        }
    }

    fn limits(managed: i64) -> Vec<MemoryLimit> {
        vec![MemoryLimit {
            domain: "unified".into(),
            managed_bytes: managed,
            free_reserve_bytes: 0,
            host_kv_bytes: None,
            parked_bytes: None,
        }]
    }

    fn ledger(owners: &[(&str, i64)]) -> LedgerSnapshot {
        LedgerSnapshot {
            epoch: 1,
            owners: owners
                .iter()
                .map(|(o, b)| (o.to_string(), footprint(*b * GIB, ResourcePhase::Ready)))
                .collect(),
        }
    }

    fn candidate(owner: &str, elsewhere: bool, used: i64) -> VictimCandidate {
        VictimCandidate {
            owner: owner.into(),
            serves_elsewhere: elsewhere,
            last_used_ms: used,
            parked: None,
        }
    }

    fn owners(victims: Vec<Victim>) -> Vec<String> {
        victims.into_iter().map(|v| v.owner).collect()
    }

    // ADR 0013 §8 rule 4: serving elsewhere first, then least recently used.
    #[test]
    fn victims_serving_elsewhere_go_first_then_least_recently_used() {
        let mut c = vec![
            candidate("c", false, 10),
            candidate("a", false, 5),
            candidate("b", true, 99),
        ];
        order_victims(&mut c);
        let order: Vec<_> = c.iter().map(|v| v.owner.as_str()).collect();
        assert_eq!(order, ["b", "a", "c"]);
    }

    // T23 T27: only the minimum set whose release makes the waiting instance
    // fit; nothing is released when it fits already.
    #[test]
    fn the_victim_set_is_minimal_and_empty_when_it_fits() {
        let l = ledger(&[("a", 8), ("b", 8)]);
        let want = footprint(10 * GIB, ResourcePhase::Cold);
        let ordered = vec![candidate("a", false, 1), candidate("b", false, 2)];
        assert_eq!(
            owners(choose_victims(&l, "w", &want, &limits(20 * GIB), 16, &ordered).unwrap()),
            ["a"]
        );
        assert!(
            choose_victims(&l, "w", &want, &limits(26 * GIB), 16, &ordered)
                .unwrap()
                .is_empty()
        );
        // A small victim preferred first does not suffice; the prune keeps
        // it serving once the larger one is released.
        let l = ledger(&[("small", 2), ("big", 8)]);
        let ordered = vec![candidate("small", true, 1), candidate("big", false, 2)];
        assert_eq!(
            owners(choose_victims(&l, "w", &want, &limits(17 * GIB), 16, &ordered).unwrap()),
            ["big"]
        );
    }

    // SPEC §7 (T23): never placed over budget; the host's reason is returned.
    #[test]
    fn no_victim_set_is_returned_when_nothing_makes_it_fit() {
        let l = ledger(&[("a", 8)]);
        let want = footprint(30 * GIB, ResourcePhase::Cold);
        let ordered = vec![candidate("a", false, 1)];
        assert_eq!(
            choose_victims(&l, "w", &want, &limits(20 * GIB), 16, &ordered),
            Err(HostRefusal::Insufficient)
        );
    }

    // Discrete GPU design §4 (T16 T27): the motivating case. Two models whose
    // device requests together exceed the card's limit, while host RAM holds
    // both, produce a victim: the device domain binds on a small card.
    #[test]
    fn the_device_domain_binds_on_a_small_card() {
        let two = |phase, device: i64, system: i64| PhaseFootprint {
            phase,
            allocations: vec![
                Allocation {
                    domain: "gpu0".into(),
                    bytes: device * GIB,
                    host_kv_bytes: 0,
                },
                Allocation {
                    domain: "system".into(),
                    bytes: system * GIB,
                    host_kv_bytes: 0,
                },
            ],
            devices: vec![],
        };
        let limit = |domain: &str, managed: i64| MemoryLimit {
            domain: domain.into(),
            managed_bytes: managed * GIB,
            free_reserve_bytes: 0,
            host_kv_bytes: None,
            parked_bytes: None,
        };
        let limits = vec![limit("gpu0", 15), limit("system", 30)];
        let l = LedgerSnapshot {
            epoch: 1,
            owners: [("a".to_string(), two(ResourcePhase::Ready, 9, 4))].into(),
        };
        let want = two(ResourcePhase::Cold, 9, 4);
        let ordered = vec![candidate("a", false, 1)];
        assert_eq!(
            owners(choose_victims(&l, "w", &want, &limits, 16, &ordered).unwrap()),
            ["a"]
        );
        // The same models on a card with room for both evict nothing.
        let roomy = vec![limit("gpu0", 18), limit("system", 30)];
        assert!(choose_victims(&l, "w", &want, &roomy, 16, &ordered)
            .unwrap()
            .is_empty());
    }

    fn two_domains(phase: ResourcePhase, device: i64, system: i64) -> PhaseFootprint {
        let allocation = |domain: &str, bytes| Allocation {
            domain: domain.into(),
            bytes,
            host_kv_bytes: 0,
        };
        PhaseFootprint {
            phase,
            allocations: vec![allocation("gpu0", device), allocation("system", system)],
            devices: vec![],
        }
    }

    fn ready_footprint(device: i64, system: i64) -> PhaseFootprint {
        two_domains(ResourcePhase::Ready, device, system)
    }

    fn cold_footprint(device: i64, system: i64) -> PhaseFootprint {
        two_domains(ResourcePhase::Cold, device, system)
    }

    fn parked_footprint(device: i64, system: i64) -> PhaseFootprint {
        two_domains(ResourcePhase::Parked, device, system)
    }

    fn ledger_with<const N: usize>(owners: [(&str, PhaseFootprint); N]) -> LedgerSnapshot {
        LedgerSnapshot {
            epoch: 1,
            owners: owners
                .into_iter()
                .map(|(owner, f)| (owner.to_string(), f))
                .collect(),
        }
    }

    /// A 16 GiB card and 61 GiB of host RAM whose parked copies are bounded
    /// by `parked` (the system domain's `parked_limit`).
    fn discrete_limits_with_system_parked(parked: i64) -> Vec<MemoryLimit> {
        vec![
            MemoryLimit {
                domain: "gpu0".into(),
                managed_bytes: 16 * GIB,
                free_reserve_bytes: 0,
                host_kv_bytes: None,
                parked_bytes: None,
            },
            MemoryLimit {
                domain: "system".into(),
                managed_bytes: 61 * GIB,
                free_reserve_bytes: 0,
                host_kv_bytes: None,
                parked_bytes: Some(parked),
            },
        ]
    }

    // Discrete GPU design §5 "When a copy does not fit" (review focus 4): a
    // host_backed victim whose weights copy would push the parked copies over
    // the system parked_limit is stopped, never parked over the limit.
    // T27
    #[test]
    fn a_copy_that_does_not_fit_is_stopped_not_parked() {
        // system parked_limit 12 GiB, one 8 GiB copy already parked.
        let limits = discrete_limits_with_system_parked(12 * GIB);
        let ledger = ledger_with([
            ("p", parked_footprint(GIB, 4 * GIB + 8 * GIB)),
            ("a", ready_footprint(12 * GIB, 4 * GIB)),
        ]);
        let a_parked = parked_footprint(GIB, 4 * GIB + 8 * GIB); // another 8 GiB copy
        let victims = [VictimCandidate {
            owner: "a".into(),
            serves_elsewhere: false,
            last_used_ms: 1,
            parked: Some(a_parked),
        }];
        let chosen = choose_victims(
            &ledger,
            "b",
            &cold_footprint(12 * GIB, 4 * GIB),
            &limits,
            4,
            &victims,
        )
        .unwrap();
        assert_eq!(
            chosen,
            vec![Victim {
                owner: "a".into(),
                release: Release::Stop
            }]
        );
    }

    // Discrete GPU design §5: a copy that fits the host after the switch
    // parks.
    // T27 T16
    #[test]
    fn a_copy_that_fits_is_parked() {
        let limits = discrete_limits_with_system_parked(24 * GIB);
        let ledger = ledger_with([("a", ready_footprint(12 * GIB, 4 * GIB))]);
        let victims = [VictimCandidate {
            owner: "a".into(),
            serves_elsewhere: false,
            last_used_ms: 1,
            parked: Some(parked_footprint(GIB, 12 * GIB)),
        }];
        let chosen = choose_victims(
            &ledger,
            "b",
            &cold_footprint(12 * GIB, 4 * GIB),
            &limits,
            4,
            &victims,
        )
        .unwrap();
        assert_eq!(chosen[0].release, Release::Park);
    }

    // SPEC §6.2: a victim that does not park (`restart_only`, no parked
    // footprint) is stopped; of two copies only one fitting, the later in
    // preference order parks and the earlier stops, and the result still fits.
    // T27
    #[test]
    fn a_victim_without_a_parked_footprint_stops_and_parking_never_overcommits() {
        let limits = discrete_limits_with_system_parked(12 * GIB);
        let ledger = ledger_with([
            ("a", ready_footprint(8 * GIB, 4 * GIB)),
            ("c", ready_footprint(7 * GIB, 4 * GIB)),
        ]);
        let want = cold_footprint(14 * GIB, 4 * GIB);
        let copy = || Some(parked_footprint(GIB, 4 * GIB + 6 * GIB));
        let candidate = |owner: &str, parked| VictimCandidate {
            owner: owner.into(),
            serves_elsewhere: false,
            last_used_ms: 1,
            parked,
        };
        let chosen = choose_victims(
            &ledger,
            "b",
            &want,
            &limits,
            4,
            &[candidate("a", copy()), candidate("c", copy())],
        )
        .unwrap();
        assert_eq!(
            chosen,
            vec![
                Victim {
                    owner: "a".into(),
                    release: Release::Stop
                },
                Victim {
                    owner: "c".into(),
                    release: Release::Park
                },
            ]
        );
        let chosen = choose_victims(
            &ledger,
            "b",
            &want,
            &limits,
            4,
            &[candidate("a", None), candidate("c", None)],
        )
        .unwrap();
        assert!(chosen.iter().all(|v| v.release == Release::Stop));
    }

    // T16 T26 (final review I4): a victim the ledger would park is stopped
    // when the observed memory cannot take its parked footprint; the victim
    // set itself is unchanged. The same rule on a unified pool.
    #[test]
    fn a_park_the_observed_memory_cannot_take_becomes_a_stop() {
        let led = ledger(&[("a", 8)]);
        let mut offered = candidate("a", false, 1);
        offered.parked = Some(footprint(4 * GIB, ResourcePhase::Parked));
        let mut limits = limits(12 * GIB);
        limits[0].free_reserve_bytes = 2 * GIB;
        let target = footprint(8 * GIB, ResourcePhase::Cold);
        let ledger_only = choose_victims(
            &led,
            "n",
            &target,
            &limits,
            4,
            std::slice::from_ref(&offered),
        )
        .unwrap();
        assert_eq!(ledger_only[0].release, Release::Park);
        let observe = |available: i64| {
            vec![mllm_domain::resources::MemoryObservation {
                domain: "unified".into(),
                capacity_bytes: 64 * GIB,
                available_bytes: available,
                sampled_at_ms: 1,
            }]
        };
        // a was sampled holding its 8 GiB: parked it keeps 4, n takes 8.
        let floors = vec![mllm_domain::resources::ResidentFloor {
            owner: "a".into(),
            domain: "unified".into(),
            bytes: 8 * GIB,
            sampled_at_ms: 1,
        }];
        let decide = |available: i64| {
            let observations = observe(available);
            let room = |state: &LedgerSnapshot| {
                observed_room(&led, &limits, &observations, &floors, state)
            };
            choose_victims_within(
                &led,
                "n",
                &target,
                &limits,
                4,
                std::slice::from_ref(&offered),
                &room,
            )
            .unwrap()
        };
        // 10 GiB free + 8 returned - 12 charged = 6 >= 2: parks.
        assert_eq!(decide(10 * GIB)[0].release, Release::Park);
        // 5 GiB free (other programs hold the rest): 1 < 2: the same victim stops.
        let stopped = decide(5 * GIB);
        assert_eq!(owners(stopped.clone()), ["a"]);
        assert_eq!(stopped[0].release, Release::Stop);
    }
}
