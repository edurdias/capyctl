use mllm_domain::resources::*;

use crate::residency::{admit_phase, validate_admission_context, AdmissionContext, ResourceError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForecastStep {
    pub owner: String,
    pub footprint: PhaseFootprint,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForecastAction {
    Phase(ForecastStep),
    RemoveAfterVerifiedCleanup { owner: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceFailure {
    pub step: usize,
    pub reason: ResourceError,
}

/// Forecasts steps by copying inputs and updating synthetic accounting only.
///
/// Forecast state is not measured evidence and does not authorize engine actions.
/// The coordinator must reobserve and revalidate resources before each real step.
pub fn forecast_sequence(
    initial: &LedgerSnapshot,
    steps: &[ForecastStep],
    context: AdmissionContext<'_>,
) -> Result<LedgerSnapshot, SequenceFailure> {
    forecast_actions(
        initial,
        &steps
            .iter()
            .cloned()
            .map(ForecastAction::Phase)
            .collect::<Vec<_>>(),
        context,
    )
}

/// SPEC §6.5 (W5): reclaim least-recently-used parked owners first. Given the
/// ledger as one host's admission sees it, the candidate's next footprint and
/// the parked owners eligible for reclamation in least-recently-used order,
/// return the shortest LRU prefix whose removal (after verified cleanup) lets
/// the candidate be admitted, or `None` when even removing every eligible
/// parked owner does not. An empty prefix means the candidate fits now.
///
/// This is a forecast only: nothing is released here, and each victim is
/// removed from the ledger only by its own verified cleanup.
pub fn lru_parked_victims(
    ledger: &LedgerSnapshot,
    candidate: &str,
    next: &PhaseFootprint,
    parked_lru: &[String],
    context: AdmissionContext<'_>,
) -> Option<Vec<String>> {
    let mut state = ledger.clone();
    let mut victims = Vec::new();
    let mut remaining = parked_lru.iter().filter(|owner| {
        owner.as_str() != candidate
            && ledger
                .owners
                .get(owner.as_str())
                .is_some_and(|f| f.phase == ResourcePhase::Parked)
    });
    loop {
        if admit_phase(&state, candidate, next, context).is_ok() {
            return Some(victims);
        }
        let victim = remaining.next()?;
        state.owners.remove(victim.as_str());
        victims.push(victim.clone());
    }
}

/// Replays phase changes and conditional cleanup using synthetic resident evidence.
/// Cleanup is only a forecast: real release still requires verified owned cleanup.
pub fn forecast_actions(
    initial: &LedgerSnapshot,
    actions: &[ForecastAction],
    context: AdmissionContext<'_>,
) -> Result<LedgerSnapshot, SequenceFailure> {
    let mut state = initial.clone();
    let mut forecast = context.observations.to_vec();
    let mut floors = context.resident_floors.to_vec();
    for (index, action) in actions.iter().enumerate() {
        let fail = |reason| SequenceFailure {
            step: index,
            reason,
        };
        let current_context = AdmissionContext {
            observations: &forecast,
            resident_floors: &floors,
            ..context
        };
        let step = match action {
            ForecastAction::Phase(step) => step,
            ForecastAction::RemoveAfterVerifiedCleanup { owner } => {
                validate_admission_context(&state, current_context).map_err(fail)?;
                if !state.owners.contains_key(owner) {
                    return Err(fail(ResourceError::Invalid));
                }
                for observation in &mut forecast {
                    let credit = floors
                        .iter()
                        .find(|f| f.owner == *owner && f.domain == observation.domain)
                        .map(|f| f.bytes)
                        .unwrap_or(0);
                    observation.available_bytes = observation
                        .available_bytes
                        .checked_add(credit)
                        .ok_or_else(|| fail(ResourceError::Invalid))?;
                }
                floors.retain(|f| f.owner != *owner);
                state.owners.remove(owner);
                continue;
            }
        };
        admit_phase(&state, &step.owner, &step.footprint, current_context).map_err(fail)?;
        for observation in &mut forecast {
            let get = |f: &PhaseFootprint| {
                f.allocations
                    .iter()
                    .find(|a| a.domain == observation.domain)
                    .map(|a| a.bytes)
                    .unwrap_or(0)
            };
            let before = floors
                .iter()
                .find(|f| f.owner == step.owner && f.domain == observation.domain)
                .map(|f| f.bytes)
                .unwrap_or(0);
            let after = get(&step.footprint);
            observation.available_bytes = observation
                .available_bytes
                .checked_add(before)
                .and_then(|x| x.checked_sub(after))
                .ok_or_else(|| fail(ResourceError::Invalid))?
                .min(observation.capacity_bytes);
            floors.retain(|f| !(f.owner == step.owner && f.domain == observation.domain));
            floors.push(ResidentFloor {
                owner: step.owner.clone(),
                domain: observation.domain.clone(),
                bytes: after,
                sampled_at_ms: observation.sampled_at_ms,
            });
        }
        state
            .owners
            .insert(step.owner.clone(), step.footprint.clone());
    }
    Ok(state)
}

#[cfg(test)]
mod lru_tests {
    use super::*;

    const GIB: i64 = 1 << 30;

    fn footprint(phase: ResourcePhase, gib: i64) -> PhaseFootprint {
        PhaseFootprint {
            phase,
            allocations: vec![Allocation {
                domain: "unified".into(),
                bytes: gib * GIB,
                host_kv_bytes: 0,
            }],
            devices: vec![],
        }
    }

    fn observed() -> Vec<MemoryObservation> {
        vec![MemoryObservation {
            domain: "unified".into(),
            capacity_bytes: 128 * GIB,
            available_bytes: 128 * GIB,
            sampled_at_ms: 1_000,
        }]
    }

    fn limits(managed: i64, parked: Option<i64>) -> Vec<MemoryLimit> {
        vec![MemoryLimit {
            domain: "unified".into(),
            managed_bytes: managed * GIB,
            free_reserve_bytes: 0,
            host_kv_bytes: None,
            parked_bytes: parked.map(|p| p * GIB),
        }]
    }

    fn ledger(owners: &[(&str, PhaseFootprint)]) -> LedgerSnapshot {
        LedgerSnapshot {
            epoch: 3,
            owners: owners
                .iter()
                .map(|(id, f)| (id.to_string(), f.clone()))
                .collect(),
        }
    }

    // T26 T27: with room, nothing is reclaimed.
    #[test]
    fn a_fitting_candidate_reclaims_nothing() {
        let ledger = ledger(&[("a", footprint(ResourcePhase::Parked, 2))]);
        let observations = observed();
        let limits = limits(32, None);
        let context = AdmissionContext::new(&observations, &limits, 1_000, 2_000, 4);
        assert_eq!(
            lru_parked_victims(
                &ledger,
                "b",
                &footprint(ResourcePhase::Cold, 10),
                &["a".into()],
                context
            ),
            Some(vec![])
        );
    }

    // T26 T27 (SPEC §6.5): the least recently used parked owners are chosen
    // first, and only as many as the candidate needs.
    #[test]
    fn the_shortest_lru_prefix_is_reclaimed() {
        let ledger = ledger(&[
            ("old", footprint(ResourcePhase::Parked, 4)),
            ("mid", footprint(ResourcePhase::Parked, 4)),
            ("new", footprint(ResourcePhase::Parked, 4)),
            ("ready", footprint(ResourcePhase::Ready, 10)),
        ]);
        let observations = observed();
        let limits = limits(28, None);
        let context = AdmissionContext::new(&observations, &limits, 1_000, 2_000, 8);
        let lru: Vec<String> = ["old", "mid", "new"].map(String::from).to_vec();
        // 22 GiB held; a 10 GiB candidate needs 4 GiB back: one victim.
        assert_eq!(
            lru_parked_victims(
                &ledger,
                "c",
                &footprint(ResourcePhase::Cold, 10),
                &lru,
                context
            ),
            Some(vec!["old".to_string()])
        );
        // 14 GiB: needs 8 GiB back, two victims, never the Ready owner.
        assert_eq!(
            lru_parked_victims(
                &ledger,
                "c",
                &footprint(ResourcePhase::Cold, 14),
                &lru,
                context
            ),
            Some(vec!["old".to_string(), "mid".to_string()])
        );
        // No eviction of Ready work: a candidate that fits only by stopping
        // the Ready owner is refused.
        assert_eq!(
            lru_parked_victims(
                &ledger,
                "c",
                &footprint(ResourcePhase::Cold, 20),
                &lru,
                context
            ),
            None
        );
    }

    // SPEC §6.5: the parked count bound is enforced when another owner parks.
    #[test]
    fn max_parked_reclaims_the_oldest_parked_owner() {
        let ledger = ledger(&[
            ("old", footprint(ResourcePhase::Parked, 1)),
            ("new", footprint(ResourcePhase::Parked, 1)),
            ("c", footprint(ResourcePhase::Ready, 8)),
        ]);
        let observations = observed();
        let limits = limits(64, None);
        let context = AdmissionContext::new(&observations, &limits, 1_000, 2_000, 2);
        let lru: Vec<String> = ["old", "new"].map(String::from).to_vec();
        assert_eq!(
            lru_parked_victims(
                &ledger,
                "c",
                &footprint(ResourcePhase::Parked, 1),
                &lru,
                context
            ),
            Some(vec!["old".to_string()])
        );
        // A parked residual budget is enforced the same way.
        let limits = self::limits(64, Some(2));
        let context = AdmissionContext::new(&observations, &limits, 1_000, 2_000, 8);
        assert_eq!(
            lru_parked_victims(
                &ledger,
                "c",
                &footprint(ResourcePhase::Parked, 1),
                &lru,
                context
            ),
            Some(vec!["old".to_string()])
        );
        // The candidate never reclaims itself, and a non-parked owner named in
        // the order is ignored.
        let lru: Vec<String> = ["c", "old", "new"].map(String::from).to_vec();
        assert_eq!(
            lru_parked_victims(
                &ledger,
                "c",
                &footprint(ResourcePhase::Parked, 1),
                &lru,
                context
            ),
            Some(vec!["old".to_string()])
        );
    }
}
