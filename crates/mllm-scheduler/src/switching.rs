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
/// lets `footprint` fit for `owner` on this host.
///
/// `Ok(vec![])` means it fits already. `Err` carries the host's reason when
/// even releasing every candidate does not make it fit. The set is minimal:
/// the shortest prefix of `ordered` that fits is taken, then every victim
/// whose release turns out unnecessary once the later ones are released is
/// kept serving, starting from the least preferred.
pub fn choose_victims(
    ledger: &LedgerSnapshot,
    owner: &str,
    footprint: &PhaseFootprint,
    limits: &[MemoryLimit],
    max_parked: usize,
    ordered: &[VictimCandidate],
) -> Result<Vec<String>, HostRefusal> {
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
    Ok(chosen)
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
        }
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
            choose_victims(&l, "w", &want, &limits(20 * GIB), 16, &ordered).unwrap(),
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
            choose_victims(&l, "w", &want, &limits(17 * GIB), 16, &ordered).unwrap(),
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
}
