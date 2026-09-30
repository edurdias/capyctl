//! Pure resource sequence planning; forecasts do not authorize runtime effects.

// The public contract carries one complete, owned diagnostic without boxing it.
#![allow(clippy::result_large_err)]

use capyctl_domain::resources::*;
use capyctl_scheduler::residency::{validate_admission_context, AdmissionContext};
pub use capyctl_scheduler::sequence::ForecastAction;
use capyctl_scheduler::sequence::{forecast_actions, ForecastStep, SequenceFailure};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerPlanSpec {
    pub recipe: RecipeFootprints,
    pub warm: bool,
    pub managed: bool,
    pub qualified_initialize: bool,
    pub qualified_park: bool,
    pub qualified_restore: bool,
    pub opposing_claim: bool,
    pub automatic_reclamation: bool,
    pub admission_window_until_ms: Option<i64>,
    pub allow_cold_stop: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalState {
    Ready,
    Parked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparationMember {
    pub owner: String,
    pub final_state: FinalState,
}

#[derive(Debug, Clone, Copy)]
pub struct PlannerInput<'a> {
    /// SPEC §7 (T26 T27): the ledger as this host's judgement sees it. A park,
    /// switch or preparation plan is judged against one host's limits, so the
    /// caller passes `Store::host_scoped_resource_snapshot` for that host's
    /// domains, never the whole multi-host ledger: another host's owners would
    /// otherwise fail `validate_admission_context` as unknown domains and count
    /// against this host's `max_parked`.
    pub initial: &'a LedgerSnapshot,
    pub owners: &'a BTreeMap<String, OwnerPlanSpec>,
    pub admission: AdmissionContext<'a>,
    pub max_expansions: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    Invalid,
    NoSafeSequence(PlanDiagnostic),
    SearchLimit { expanded: usize, limit: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanDiagnostic {
    /// Requested owner being forecast, not an assertion about a unique cause.
    pub target: String,
    /// Required forecast action at the denial; this is not measured usage.
    pub failed_step: Option<ForecastAction>,
    pub reason: Option<ResourceError>,
    pub attempted_prefix: Vec<ForecastAction>,
    /// Original supplied observations, never synthetic post-release availability.
    pub observations: Vec<MemoryObservation>,
    pub limits: Vec<MemoryLimit>,
    pub excluded: BTreeMap<String, Exclusion>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exclusion {
    Attached,
    OpposingClaim,
    AutomaticReclamationDisabled,
    AdmissionWindowOpen,
    UnqualifiedInitialize,
    UnqualifiedPark,
    UnqualifiedRestore,
    TransientPhase,
    ColdStopDisabled,
    WarmResidencyRequired,
}

/// Plans an activation without changing reservations, admission windows, or engines.
pub fn plan_activation(
    input: PlannerInput<'_>,
    target: &str,
) -> Result<Vec<ForecastAction>, PlanError> {
    validate_input(input, std::iter::once(target))?;
    let mut search = Search::new(input, target);
    if let Some(exclusion) = requested_gate(input, target, false) {
        search.diagnostic.excluded.insert(target.into(), exclusion);
        return Err(PlanError::NoSafeSequence(search.diagnostic));
    }
    search.run(Goal::Activation(target))
}

/// Preserves initialization order and forecasts the exact requested warm arrangement.
pub fn plan_preparation(
    input: PlannerInput<'_>,
    members: &[PreparationMember],
) -> Result<Vec<ForecastAction>, PlanError> {
    if members.is_empty() || members.len() > 1024 {
        return Err(PlanError::Invalid);
    }
    let members = members.to_vec();
    validate_input(input, members.iter().map(|m| m.owner.as_str()))?;
    let mut search = Search::new(input, &members[0].owner);
    for member in &members {
        if let Some(exclusion) = requested_gate(input, &member.owner, true) {
            if search.diagnostic.excluded.is_empty() {
                search.diagnostic.target.clone_from(&member.owner);
            }
            search
                .diagnostic
                .excluded
                .insert(member.owner.clone(), exclusion);
        }
    }
    if !search.diagnostic.excluded.is_empty() {
        return Err(PlanError::NoSafeSequence(search.diagnostic));
    }
    search.run(Goal::Preparation(&members))
}

pub fn may_reclaim(warm: bool, qualified_park: bool, allow_cold_stop: bool) -> bool {
    qualified_park || (!warm && allow_cold_stop)
}

fn validate_input<'a>(
    input: PlannerInput<'_>,
    requested: impl Iterator<Item = &'a str>,
) -> Result<(), PlanError> {
    if input.max_expansions == 0 || input.owners.len() > 1024 {
        return Err(PlanError::Invalid);
    }
    let mut unique = BTreeSet::new();
    for owner in requested {
        if owner.is_empty() || !unique.insert(owner) || !input.owners.contains_key(owner) {
            return Err(PlanError::Invalid);
        }
    }
    for (owner, spec) in input.owners {
        if owner.is_empty() || spec.admission_window_until_ms.is_some_and(|t| t < 0) {
            return Err(PlanError::Invalid);
        }
        validate_recipe(&spec.recipe).map_err(|_| PlanError::Invalid)?;
        if let Some(retained) = input.initial.owners.get(owner) {
            let expected = match retained.phase {
                ResourcePhase::Ready => Some(&spec.recipe.ready),
                ResourcePhase::Parked => Some(&spec.recipe.parked),
                _ => None,
            };
            if expected.is_some_and(|f| !same_footprint(f, retained)) {
                return Err(PlanError::Invalid);
            }
        }
    }
    for (owner, footprint) in &input.initial.owners {
        if owner.is_empty() {
            return Err(PlanError::Invalid);
        }
        validate_footprint(footprint).map_err(|_| PlanError::Invalid)?;
    }
    Ok(())
}

// Domain and device order is serialization detail; all validated accounting fields
// must match exactly. Never normalize or rewrite the caller's reviewed recipe.
fn same_footprint(left: &PhaseFootprint, right: &PhaseFootprint) -> bool {
    left.phase == right.phase
        && left.allocations.len() == right.allocations.len()
        && left
            .allocations
            .iter()
            .all(|a| right.allocations.contains(a))
        && left.devices.len() == right.devices.len()
        && left.devices.iter().all(|d| right.devices.contains(d))
}

fn ownership_gate(spec: &OwnerPlanSpec) -> Option<Exclusion> {
    if !spec.managed {
        Some(Exclusion::Attached)
    } else if spec.opposing_claim {
        Some(Exclusion::OpposingClaim)
    } else {
        None
    }
}

fn requested_gate(input: PlannerInput<'_>, owner: &str, preparation: bool) -> Option<Exclusion> {
    let spec = &input.owners[owner];
    if let Some(exclusion) = ownership_gate(spec) {
        return Some(exclusion);
    }
    match input.initial.owners.get(owner).map(|f| f.phase) {
        None if !spec.qualified_initialize => return Some(Exclusion::UnqualifiedInitialize),
        Some(ResourcePhase::Parked) if !spec.qualified_restore => {
            return Some(Exclusion::UnqualifiedRestore)
        }
        Some(ResourcePhase::Cold | ResourcePhase::Parking | ResourcePhase::Wake) => {
            return Some(Exclusion::TransientPhase)
        }
        _ => {}
    }
    if preparation {
        if !spec.warm {
            return Some(Exclusion::WarmResidencyRequired);
        }
        if !spec.qualified_park {
            return Some(Exclusion::UnqualifiedPark);
        }
        if !spec.qualified_restore {
            return Some(Exclusion::UnqualifiedRestore);
        }
    }
    None
}

fn victim_gate(spec: &OwnerPlanSpec, now_ms: i64, explicit_final_park: bool) -> Option<Exclusion> {
    if let Some(exclusion) = ownership_gate(spec) {
        return Some(exclusion);
    }
    if !explicit_final_park {
        if !spec.automatic_reclamation {
            return Some(Exclusion::AutomaticReclamationDisabled);
        }
        if spec
            .admission_window_until_ms
            .is_some_and(|until| until > now_ms)
        {
            return Some(Exclusion::AdmissionWindowOpen);
        }
    }
    None
}

#[derive(Clone)]
struct Node {
    state: LedgerSnapshot,
    prefix: Vec<ForecastAction>,
    touched: BTreeSet<String>,
    cursor: usize,
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    phases: Vec<(String, u8)>,
    touched: BTreeSet<String>,
    cursor: usize,
}

// A touched owner's floor equals its final recipe allocation; an untouched owner's
// floor is the original supplied floor. A removed owner has no floor. Availability
// telescopes as original availability + original floors - current floors. Admission
// checks total floors <= observed used bytes before each step, so the forecaster's
// capacity clamp cannot discard credit. Thus mapping + touches determines the same
// synthetic observations and floors, independent of path; cursor fixes remaining
// initialization order. All immutable owners keep their original full footprint.

impl Node {
    fn key(&self) -> Key {
        Key {
            phases: self
                .state
                .owners
                .iter()
                .map(|(id, f)| {
                    (
                        id.clone(),
                        match f.phase {
                            ResourcePhase::Cold => 0,
                            ResourcePhase::Ready => 1,
                            ResourcePhase::Parking => 2,
                            ResourcePhase::Parked => 3,
                            ResourcePhase::Wake => 4,
                        },
                    )
                })
                .collect(),
            touched: self.touched.clone(),
            cursor: self.cursor,
        }
    }
}

#[derive(Clone, Copy)]
enum Goal<'a> {
    Activation(&'a str),
    Preparation(&'a [PreparationMember]),
}

impl Goal<'_> {
    fn advance(self, node: &mut Node) {
        if let Self::Preparation(members) = self {
            while node.cursor < members.len()
                && node.state.owners.contains_key(&members[node.cursor].owner)
            {
                node.cursor += 1;
            }
        }
    }
}

struct Search<'a> {
    input: PlannerInput<'a>,
    diagnostic: PlanDiagnostic,
    frontier: VecDeque<Node>,
    visited: BTreeSet<Key>,
    admitted: usize,
    expanded: usize,
    budget_pruned: bool,
    final_arrangement: bool,
    final_denial_recorded: bool,
    #[cfg(test)]
    high_water: [usize; 3],
    #[cfg(test)]
    admitted_owners: Vec<String>,
}

impl<'a> Search<'a> {
    fn new(input: PlannerInput<'a>, target: &str) -> Self {
        Self {
            input,
            diagnostic: PlanDiagnostic {
                target: target.into(),
                failed_step: None,
                reason: None,
                attempted_prefix: vec![],
                observations: input.admission.observations.to_vec(),
                limits: input.admission.limits.to_vec(),
                excluded: BTreeMap::new(),
            },
            frontier: VecDeque::new(),
            visited: BTreeSet::new(),
            admitted: 0,
            expanded: 0,
            budget_pruned: false,
            final_arrangement: false,
            final_denial_recorded: false,
            #[cfg(test)]
            high_water: [0; 3],
            #[cfg(test)]
            admitted_owners: vec![],
        }
    }

    fn record_failure(
        &mut self,
        owner: &str,
        prefix: &[ForecastAction],
        failure: SequenceFailure,
    ) -> Result<(), PlanError> {
        if failure.reason == ResourceError::Invalid {
            return Err(PlanError::Invalid);
        }
        if self.diagnostic.reason.is_none()
            || (self.final_arrangement && !self.final_denial_recorded)
        {
            self.diagnostic.target = owner.into();
            self.diagnostic.failed_step = prefix.get(failure.step).cloned();
            self.diagnostic.reason = Some(failure.reason);
            self.diagnostic.attempted_prefix = prefix.to_vec();
            self.final_denial_recorded |= self.final_arrangement;
        }
        Ok(())
    }

    fn replay(
        &mut self,
        owner: &str,
        prefix: &[ForecastAction],
        diagnose: bool,
    ) -> Result<Option<LedgerSnapshot>, PlanError> {
        match forecast_actions(self.input.initial, prefix, self.input.admission) {
            Ok(state) => Ok(Some(state)),
            Err(failure) => {
                if failure.reason == ResourceError::Invalid {
                    return Err(PlanError::Invalid);
                }
                if diagnose {
                    self.record_failure(owner, prefix, failure)?;
                }
                Ok(None)
            }
        }
    }

    fn pair(&self, node: &Node, owner: &str, peak: ResourcePhase) -> Vec<ForecastAction> {
        let recipe = &self.input.owners[owner].recipe;
        let (first, last) = match peak {
            ResourcePhase::Cold => (&recipe.cold, &recipe.ready),
            ResourcePhase::Parking => (&recipe.parking, &recipe.parked),
            ResourcePhase::Wake => (&recipe.wake, &recipe.ready),
            _ => unreachable!("search edges always include a peak and a settled phase"),
        };
        let mut prefix = node.prefix.clone();
        prefix.extend([first, last].map(|footprint| {
            ForecastAction::Phase(ForecastStep {
                owner: owner.into(),
                footprint: footprint.clone(),
            })
        }));
        prefix
    }

    fn enqueue(
        &mut self,
        node: &Node,
        owner: &str,
        prefix: Vec<ForecastAction>,
        goal: Goal<'_>,
        diagnose: bool,
    ) -> Result<(), PlanError> {
        if let Some(state) = self.replay(owner, &prefix, diagnose)? {
            let mut next = Node {
                state,
                prefix,
                touched: node.touched.clone(),
                cursor: node.cursor,
            };
            next.touched.insert(owner.into());
            goal.advance(&mut next);
            let key = next.key();
            if self.visited.contains(&key) {
                return Ok(());
            }
            if self.admitted == self.input.max_expansions {
                self.budget_pruned = true;
                return Ok(());
            }
            self.admitted += 1;
            self.visited.insert(key);
            self.frontier.push_back(next);
            #[cfg(test)]
            self.admitted_owners.push(owner.into());
            self.check_bounds();
        }
        Ok(())
    }

    fn check_bounds(&mut self) {
        debug_assert!(self.admitted <= self.input.max_expansions);
        debug_assert!(self.expanded <= self.admitted);
        debug_assert!(self.visited.len() <= self.input.max_expansions);
        debug_assert!(self.frontier.len() <= self.input.max_expansions);
        #[cfg(test)]
        for (peak, current) in
            self.high_water
                .iter_mut()
                .zip([self.admitted, self.frontier.len(), self.visited.len()])
        {
            *peak = (*peak).max(current);
        }
    }

    fn reclaim(
        &mut self,
        node: &Node,
        owner: &str,
        goal: Goal<'_>,
        explicit: bool,
    ) -> Result<(), PlanError> {
        let Some(spec) = self.input.owners.get(owner) else {
            return Ok(());
        };
        let phase = node.state.owners[owner].phase;
        let exclusion = victim_gate(spec, self.input.admission.now_ms, explicit).or_else(|| {
            if !matches!(phase, ResourcePhase::Ready | ResourcePhase::Parked) {
                Some(Exclusion::TransientPhase)
            } else if !may_reclaim(spec.warm, spec.qualified_park, spec.allow_cold_stop) {
                Some(if spec.warm {
                    Exclusion::UnqualifiedPark
                } else {
                    Exclusion::ColdStopDisabled
                })
            } else {
                None
            }
        });
        if let Some(exclusion) = exclusion {
            self.diagnostic
                .excluded
                .entry(owner.into())
                .or_insert(exclusion);
            return Ok(());
        }
        if phase == ResourcePhase::Ready && spec.qualified_park {
            let prefix = self.pair(node, owner, ResourcePhase::Parking);
            self.enqueue(node, owner, prefix, goal, explicit)?;
        }
        if matches!(goal, Goal::Activation(_)) && !spec.warm && spec.allow_cold_stop {
            let mut prefix = node.prefix.clone();
            prefix.push(ForecastAction::RemoveAfterVerifiedCleanup {
                owner: owner.into(),
            });
            self.enqueue(node, owner, prefix, goal, false)?;
        }
        Ok(())
    }

    fn run(&mut self, goal: Goal<'_>) -> Result<Vec<ForecastAction>, PlanError> {
        let mut root = Node {
            state: self.input.initial.clone(),
            prefix: vec![],
            touched: BTreeSet::new(),
            cursor: 0,
        };
        goal.advance(&mut root);
        self.visited.insert(root.key());
        self.frontier.push_back(root);
        self.admitted = 1;
        self.check_bounds();
        while let Some(node) = self.frontier.pop_front() {
            self.expanded += 1;
            self.check_bounds();
            self.final_arrangement =
                matches!(goal, Goal::Preparation(members) if node.cursor == members.len());
            match goal {
                Goal::Activation(target) => {
                    let phase = node.state.owners.get(target).map(|f| f.phase);
                    if phase == Some(ResourcePhase::Ready) {
                        match validate_admission_context(self.input.initial, self.input.admission) {
                            Ok(()) => return Ok(node.prefix),
                            Err(reason) => {
                                self.record_failure(
                                    target,
                                    &[],
                                    SequenceFailure { step: 0, reason },
                                )?;
                                break;
                            }
                        }
                    }
                    let prefix = self.pair(
                        &node,
                        target,
                        if phase.is_none() {
                            ResourcePhase::Cold
                        } else {
                            ResourcePhase::Wake
                        },
                    );
                    if self.replay(target, &prefix, true)?.is_some() {
                        return Ok(prefix);
                    }
                    for owner in node
                        .state
                        .owners
                        .keys()
                        .filter(|owner| owner.as_str() != target)
                    {
                        self.reclaim(&node, owner, goal, false)?;
                    }
                }
                Goal::Preparation(members) => {
                    if node.cursor < members.len() {
                        let owner = &members[node.cursor].owner;
                        let prefix = self.pair(&node, owner, ResourcePhase::Cold);
                        self.enqueue(&node, owner, prefix, goal, true)?;
                        // Only already prepared members may make space for the next initialization.
                        for owner in node.state.owners.keys() {
                            if let Some(member) =
                                members[..node.cursor].iter().find(|m| m.owner == *owner)
                            {
                                if node.state.owners[owner].phase == ResourcePhase::Ready {
                                    self.reclaim(
                                        &node,
                                        owner,
                                        goal,
                                        member.final_state == FinalState::Parked,
                                    )?;
                                }
                            }
                        }
                    } else {
                        let exact = members.iter().all(|m| {
                            node.state.owners[&m.owner].phase
                                == match m.final_state {
                                    FinalState::Ready => ResourcePhase::Ready,
                                    FinalState::Parked => ResourcePhase::Parked,
                                }
                        });
                        if exact && self.usable(&node, members)? {
                            return Ok(node.prefix);
                        }
                        // Stable identity order, with parking before wake for each owner.
                        for owner in node.state.owners.keys() {
                            let Some(member) = members.iter().find(|m| m.owner == *owner) else {
                                continue;
                            };
                            match node.state.owners[owner].phase {
                                ResourcePhase::Ready => self.reclaim(
                                    &node,
                                    owner,
                                    goal,
                                    member.final_state == FinalState::Parked,
                                )?,
                                ResourcePhase::Parked
                                    if member.final_state == FinalState::Ready =>
                                {
                                    let prefix = self.pair(&node, owner, ResourcePhase::Wake);
                                    self.enqueue(&node, owner, prefix, goal, true)?;
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
        if self.budget_pruned {
            Err(PlanError::SearchLimit {
                expanded: self.expanded,
                limit: self.input.max_expansions,
            })
        } else {
            Err(PlanError::NoSafeSequence(self.diagnostic.clone()))
        }
    }

    fn usable(&mut self, node: &Node, members: &[PreparationMember]) -> Result<bool, PlanError> {
        if node.prefix.is_empty() {
            if let Err(reason) =
                validate_admission_context(self.input.initial, self.input.admission)
            {
                self.record_failure(&members[0].owner, &[], SequenceFailure { step: 0, reason })?;
                return Ok(false);
            }
        }
        if members.iter().all(|m| m.final_state == FinalState::Parked) {
            for member in members {
                let prefix = self.pair(node, &member.owner, ResourcePhase::Wake);
                if self.replay(&member.owner, &prefix, true)?.is_none() {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lazy_successor_admission_bounds_all_search_collections() {
        let footprint = |phase, bytes| PhaseFootprint {
            phase,
            allocations: vec![Allocation {
                domain: "ram".into(),
                bytes,
                host_kv_bytes: 0,
            }],
            devices: vec![],
        };
        let victim = OwnerPlanSpec {
            recipe: RecipeFootprints {
                cold: footprint(ResourcePhase::Cold, 10),
                ready: footprint(ResourcePhase::Ready, 10),
                parking: footprint(ResourcePhase::Parking, 10),
                parked: footprint(ResourcePhase::Parked, 1),
                wake: footprint(ResourcePhase::Wake, 10),
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
        };
        let mut owners: BTreeMap<_, _> = ["A", "B", "C", "D"]
            .map(|id| (id.to_string(), victim.clone()))
            .into();
        let initial = LedgerSnapshot {
            epoch: 1,
            owners: owners
                .keys()
                .map(|id| (id.clone(), victim.recipe.ready.clone()))
                .collect(),
        };
        let mut target = victim.clone();
        target.recipe.cold = footprint(ResourcePhase::Cold, 95);
        owners.insert("Z".into(), target);
        let observations = [MemoryObservation {
            domain: "ram".into(),
            capacity_bytes: 100,
            available_bytes: 60,
            sampled_at_ms: 100,
        }];
        let floors: Vec<_> = initial
            .owners
            .keys()
            .map(|id| ResidentFloor {
                owner: id.clone(),
                domain: "ram".into(),
                bytes: 10,
                sampled_at_ms: 100,
            })
            .collect();
        let limits = [MemoryLimit {
            domain: "ram".into(),
            managed_bytes: 100,
            free_reserve_bytes: 0,
            host_kv_bytes: None,
            parked_bytes: None,
        }];
        let input = PlannerInput {
            initial: &initial,
            owners: &owners,
            admission: AdmissionContext::new(&observations, &limits, 100, 10, 10)
                .with_resident_floors(&floors),
            max_expansions: 3,
        };
        let mut search = Search::new(input, "Z");
        assert_eq!(
            search.run(Goal::Activation("Z")),
            Err(PlanError::SearchLimit {
                expanded: 3,
                limit: 3
            })
        );
        assert_eq!(search.high_water, [3, 2, 3]);
        assert_eq!(search.admitted_owners, ["A", "B"]);
        assert_eq!(search.visited.len(), 3);
    }
}
