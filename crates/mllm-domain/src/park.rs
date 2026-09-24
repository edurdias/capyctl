//! What "parked" means, and the order in which a park or a restore is performed.
//!
//! ADR 0011: parking is an ordinary transition and needs no qualification, but the
//! machine must not be trusted to have released memory because an engine said so.
//! The rules here are the guard: a park is complete only when a fresh local
//! observation shows nothing resident, nothing running and the same owned
//! processes, and a restore may only be armed on the strength of a park whose
//! evidence committed. Nothing here touches a database; the store applies these
//! rules inside its transactions.

use crate::completion::{Milestone, ProcessIdentity};
use crate::resources::PhaseFootprint;

/// The two park-family actions the ordinary lifecycle will perform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParkAction {
    Park,
    Restore,
}

/// One engine effect. Each is its own persisted step with its own evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    Drain,
    Park,
    Restore,
    ReloadWeights,
    InvalidateCache,
    Probe,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParkError {
    /// A probe is a read; it proves usability through the completion path, not facts.
    ProbeCarriesNoFacts,
    /// An earlier effect is missing, out of order, wrong, or did not advance the epoch.
    PredecessorMismatch,
    /// A phase peak names a domain the owner does not hold.
    UnknownDomain,
    /// The observation does not show a parked runtime.
    NotParked(&'static str),
}

/// The fixed order of effects for an action.
pub fn effects(action: ParkAction) -> &'static [Effect] {
    match action {
        ParkAction::Park => &[Effect::Drain, Effect::Park],
        ParkAction::Restore => &[
            Effect::Restore,
            Effect::ReloadWeights,
            Effect::InvalidateCache,
            Effect::Probe,
        ],
    }
}

/// The facts one committed effect establishes.
pub fn facts(effect: Effect) -> Result<Vec<Milestone>, ParkError> {
    Ok(match effect {
        Effect::Drain => vec![Milestone::Quiesced],
        Effect::Park => vec![Milestone::MemoryReleased],
        Effect::Restore => vec![Milestone::AllocationsRestored],
        Effect::ReloadWeights => vec![Milestone::WeightsUsable],
        Effect::InvalidateCache => vec![Milestone::CacheValid],
        Effect::Probe => return Err(ParkError::ProbeCarriesNoFacts),
    })
}

/// What the store reads back for an earlier effect of the same action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommittedEffect {
    pub effect: Effect,
    pub facts: Vec<Milestone>,
    pub observed_at_ms: i64,
    pub committed_epoch: u64,
}

/// Before arming effect number `ordinal` (1-based) of `action`, every earlier
/// effect must have committed with exactly its facts, observed no earlier than the
/// action's acceptance and no later than `observed_at_ms`, at strictly increasing
/// epochs. Returns the accumulated facts on success.
pub fn validate_predecessors(
    action: ParkAction,
    ordinal: usize,
    committed: &[CommittedEffect],
    accepted_at_ms: i64,
    observed_at_ms: i64,
) -> Result<Vec<Milestone>, ParkError> {
    let expected = effects(action);
    if ordinal == 0 || ordinal > expected.len() || committed.len() != ordinal - 1 {
        return Err(ParkError::PredecessorMismatch);
    }
    let mut collected = Vec::new();
    let mut previous_time = accepted_at_ms;
    let mut previous_epoch = 0;
    for (effect, evidence) in expected.iter().zip(committed) {
        if evidence.effect != *effect
            || evidence.facts != facts(*effect)?
            || evidence.observed_at_ms < previous_time
            || evidence.observed_at_ms > observed_at_ms
            || evidence.committed_epoch <= previous_epoch
        {
            return Err(ParkError::PredecessorMismatch);
        }
        previous_time = evidence.observed_at_ms;
        previous_epoch = evidence.committed_epoch;
        collected.extend(evidence.facts.iter().copied());
    }
    Ok(collected)
}

/// Join a phase peak into the owner's retained footprint. Bytes and host KV take
/// the maximum per domain; device sharing can only escalate to exclusive. The
/// retained reservation never shrinks here, because a smaller number would be a
/// claim about released memory that only a parked-status observation can make.
pub fn join(base: &mut PhaseFootprint, peak: &PhaseFootprint) -> Result<(), ParkError> {
    for next in &peak.allocations {
        let old = base
            .allocations
            .iter_mut()
            .find(|a| a.domain == next.domain)
            .ok_or(ParkError::UnknownDomain)?;
        old.bytes = old.bytes.max(next.bytes);
        old.host_kv_bytes = old.host_kv_bytes.max(next.host_kv_bytes);
    }
    for next in &peak.devices {
        if let Some(old) = base.devices.iter_mut().find(|d| d.device == next.device) {
            if next.sharing == crate::resources::Sharing::Exclusive {
                old.sharing = next.sharing;
            }
        } else {
            base.devices.push(next.clone());
        }
    }
    Ok(())
}

/// A local, read-only observation of a runtime believed parked. No engine command
/// and no inference request is issued to take it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParkedStatus {
    pub identities: Vec<ProcessIdentity>,
    pub observed_at_ms: i64,
    pub receipt: String,
    pub allocations: bool,
    pub weights: bool,
    pub cache: bool,
    pub quiesced: bool,
    pub unknown_work: bool,
    pub activity_before: (u64, u64, u64),
    pub activity_after: (u64, u64, u64),
}

/// What the store knows independently of the observation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParkedExpectation {
    /// The processes recorded in the owned-launch association.
    pub owned: Vec<ProcessIdentity>,
    /// When the Park effect's own evidence was observed.
    pub park_committed_at_ms: i64,
    /// Rows in `request_leases` for this deployment.
    pub outstanding_request_leases: u64,
}

/// SPEC §6.1 PARKED: "release conditions verified". This is the verification.
pub fn verify_parked(status: &ParkedStatus, expected: &ParkedExpectation) -> Result<(), ParkError> {
    let mut observed = status.identities.clone();
    observed.sort();
    let mut owned = expected.owned.clone();
    owned.sort();
    if observed.is_empty() || observed != owned {
        return Err(ParkError::NotParked(
            "identities differ from the owned association",
        ));
    }
    if status.allocations {
        return Err(ParkError::NotParked("allocations still resident"));
    }
    if status.weights {
        return Err(ParkError::NotParked("weights still resident"));
    }
    if status.cache {
        return Err(ParkError::NotParked("cache still resident"));
    }
    if !status.quiesced {
        return Err(ParkError::NotParked("engine not quiesced"));
    }
    if status.unknown_work {
        return Err(ParkError::NotParked("unknown work observed"));
    }
    if status.activity_before != status.activity_after {
        return Err(ParkError::NotParked(
            "activity counters moved during observation",
        ));
    }
    if status.receipt.is_empty() {
        return Err(ParkError::NotParked("empty receipt"));
    }
    if status.observed_at_ms < expected.park_committed_at_ms {
        return Err(ParkError::NotParked("observation predates the park effect"));
    }
    if expected.outstanding_request_leases != 0 {
        return Err(ParkError::NotParked("request leases outstanding"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::{Milestone, ProcessIdentity};

    fn identity() -> ProcessIdentity {
        ProcessIdentity {
            role: "api".into(),
            pid: 7,
            boot_id: "boot".into(),
            start_ticks: 1,
        }
    }

    fn parked() -> ParkedStatus {
        ParkedStatus {
            identities: vec![identity()],
            observed_at_ms: 2_000,
            receipt: "status".into(),
            allocations: false,
            weights: false,
            cache: false,
            quiesced: true,
            unknown_work: false,
            activity_before: (1, 1, 1),
            activity_after: (1, 1, 1),
        }
    }

    fn owned() -> ParkedExpectation {
        ParkedExpectation {
            owned: vec![identity()],
            park_committed_at_ms: 1_500,
            outstanding_request_leases: 0,
        }
    }

    /// The park effect sequence is fixed: drain, then park. A restore reloads
    /// weights and invalidates cache before it probes.
    #[test]
    fn effects_are_ordered_and_each_yields_its_facts() {
        assert_eq!(effects(ParkAction::Park), [Effect::Drain, Effect::Park]);
        assert_eq!(
            effects(ParkAction::Restore),
            [
                Effect::Restore,
                Effect::ReloadWeights,
                Effect::InvalidateCache,
                Effect::Probe
            ]
        );
        assert_eq!(facts(Effect::Drain), Ok(vec![Milestone::Quiesced]));
        assert_eq!(facts(Effect::Park), Ok(vec![Milestone::MemoryReleased]));
        assert_eq!(
            facts(Effect::Restore),
            Ok(vec![Milestone::AllocationsRestored])
        );
        assert_eq!(
            facts(Effect::ReloadWeights),
            Ok(vec![Milestone::WeightsUsable])
        );
        assert_eq!(
            facts(Effect::InvalidateCache),
            Ok(vec![Milestone::CacheValid])
        );
        assert_eq!(facts(Effect::Probe), Err(ParkError::ProbeCarriesNoFacts));
    }

    /// A later effect is armed only if every earlier effect committed the exact
    /// facts expected, in time order, at strictly increasing epochs. T20
    // T20
    #[test]
    fn predecessors_must_have_committed_in_order() {
        let drained = CommittedEffect {
            effect: Effect::Drain,
            facts: vec![Milestone::Quiesced],
            observed_at_ms: 1_000,
            committed_epoch: 5,
        };
        assert_eq!(
            validate_predecessors(
                ParkAction::Park,
                2,
                std::slice::from_ref(&drained),
                900,
                1_300
            ),
            Ok(vec![Milestone::Quiesced])
        );
        // Epoch did not advance.
        let stale = CommittedEffect {
            committed_epoch: 0,
            ..drained.clone()
        };
        assert_eq!(
            validate_predecessors(ParkAction::Park, 2, &[stale], 900, 1_300).unwrap_err(),
            ParkError::PredecessorMismatch
        );
        // Observed after the effect being armed.
        assert_eq!(
            validate_predecessors(
                ParkAction::Park,
                2,
                std::slice::from_ref(&drained),
                900,
                999
            )
            .unwrap_err(),
            ParkError::PredecessorMismatch
        );
        // Wrong facts for the effect.
        let wrong = CommittedEffect {
            facts: vec![Milestone::MemoryReleased],
            ..drained
        };
        assert_eq!(
            validate_predecessors(ParkAction::Park, 2, &[wrong], 900, 1_300).unwrap_err(),
            ParkError::PredecessorMismatch
        );
    }

    /// A footprint join never shrinks the retained reservation and never drops a
    /// domain the owner already holds.
    #[test]
    fn join_only_grows() {
        use crate::resources::{Allocation, PhaseFootprint, ResourcePhase};
        let mut base = PhaseFootprint {
            phase: ResourcePhase::Ready,
            allocations: vec![Allocation {
                domain: "unified".into(),
                bytes: 10,
                host_kv_bytes: 2,
            }],
            devices: vec![],
        };
        let peak = PhaseFootprint {
            phase: ResourcePhase::Parking,
            allocations: vec![Allocation {
                domain: "unified".into(),
                bytes: 4,
                host_kv_bytes: 8,
            }],
            devices: vec![],
        };
        join(&mut base, &peak).unwrap();
        assert_eq!(base.allocations[0].bytes, 10);
        assert_eq!(base.allocations[0].host_kv_bytes, 8);
        let foreign = PhaseFootprint {
            phase: ResourcePhase::Parking,
            allocations: vec![Allocation {
                domain: "other".into(),
                bytes: 1,
                host_kv_bytes: 0,
            }],
            devices: vec![],
        };
        assert_eq!(
            join(&mut base, &foreign).unwrap_err(),
            ParkError::UnknownDomain
        );
    }

    /// The parked predicate: nothing resident, nothing running, same processes,
    /// no work leased, observed after the park committed. Any one failing means
    /// the deployment is not parked, whatever the engine reported. T20
    // T20
    #[test]
    fn parked_requires_every_condition() {
        assert_eq!(verify_parked(&parked(), &owned()), Ok(()));
        let cases: Vec<(&str, ParkedStatus, ParkedExpectation)> = vec![
            (
                "allocations",
                ParkedStatus {
                    allocations: true,
                    ..parked()
                },
                owned(),
            ),
            (
                "weights",
                ParkedStatus {
                    weights: true,
                    ..parked()
                },
                owned(),
            ),
            (
                "cache",
                ParkedStatus {
                    cache: true,
                    ..parked()
                },
                owned(),
            ),
            (
                "quiesced",
                ParkedStatus {
                    quiesced: false,
                    ..parked()
                },
                owned(),
            ),
            (
                "unknown work",
                ParkedStatus {
                    unknown_work: true,
                    ..parked()
                },
                owned(),
            ),
            (
                "activity",
                ParkedStatus {
                    activity_after: (2, 1, 1),
                    ..parked()
                },
                owned(),
            ),
            (
                "receipt",
                ParkedStatus {
                    receipt: String::new(),
                    ..parked()
                },
                owned(),
            ),
            (
                "stale",
                ParkedStatus {
                    observed_at_ms: 1_000,
                    ..parked()
                },
                owned(),
            ),
            (
                "identities",
                ParkedStatus {
                    identities: vec![],
                    ..parked()
                },
                owned(),
            ),
            (
                "leases",
                parked(),
                ParkedExpectation {
                    outstanding_request_leases: 1,
                    ..owned()
                },
            ),
        ];
        for (name, status, expectation) in cases {
            assert!(
                verify_parked(&status, &expectation).is_err(),
                "{name} must refuse"
            );
        }
    }
}
