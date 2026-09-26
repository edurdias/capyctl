use std::collections::BTreeSet;

pub use mllm_domain::resources::{
    claims_conflict, validate_footprint, validate_recipe, ResourceError,
};
use mllm_domain::resources::{
    LedgerSnapshot, MemoryLimit, MemoryObservation, PhaseFootprint, ResidentFloor, ResourcePhase,
};

#[derive(Debug, Clone, Copy)]
pub struct AdmissionContext<'a> {
    pub observations: &'a [MemoryObservation],
    pub resident_floors: &'a [ResidentFloor],
    pub limits: &'a [MemoryLimit],
    pub now_ms: i64,
    pub ttl_ms: i64,
    pub max_parked: usize,
}

impl<'a> AdmissionContext<'a> {
    pub fn new(
        observations: &'a [MemoryObservation],
        limits: &'a [MemoryLimit],
        now_ms: i64,
        ttl_ms: i64,
        max_parked: usize,
    ) -> Self {
        Self {
            observations,
            resident_floors: &[],
            limits,
            now_ms,
            ttl_ms,
            max_parked,
        }
    }
    pub fn with_resident_floors(mut self, floors: &'a [ResidentFloor]) -> Self {
        self.resident_floors = floors;
        self
    }
}

fn add(a: i64, b: i64) -> Result<i64, ResourceError> {
    a.checked_add(b).ok_or(ResourceError::Invalid)
}
fn amount(f: &PhaseFootprint, domain: &str) -> (i64, i64) {
    f.allocations
        .iter()
        .find(|a| a.domain == domain)
        .map(|a| (a.bytes, a.host_kv_bytes))
        .unwrap_or((0, 0))
}

fn validate_context(
    snapshot: &LedgerSnapshot,
    context: AdmissionContext<'_>,
) -> Result<(), ResourceError> {
    let AdmissionContext {
        observations,
        resident_floors,
        limits,
        now_ms,
        ttl_ms,
        ..
    } = context;
    if ttl_ms <= 0 || now_ms < 0 || limits.is_empty() {
        return Err(ResourceError::Invalid);
    }
    for (id, f) in &snapshot.owners {
        if id.is_empty() {
            return Err(ResourceError::Invalid);
        }
        validate_footprint(f)?;
    }
    let mut known = BTreeSet::new();
    for l in limits {
        if l.domain.is_empty()
            || !known.insert(l.domain.as_str())
            || l.managed_bytes < 0
            || l.free_reserve_bytes < 0
            || l.host_kv_bytes.is_some_and(|x| x < 0)
            || l.parked_bytes.is_some_and(|x| x < 0)
        {
            return Err(ResourceError::Invalid);
        }
    }
    let mut observed = BTreeSet::new();
    for o in observations {
        if !observed.insert(o.domain.as_str()) {
            return Err(ResourceError::Invalid);
        }
        if !known.contains(o.domain.as_str()) {
            return Err(ResourceError::UnknownDomain);
        }
        if o.capacity_bytes < 0
            || o.available_bytes < 0
            || o.available_bytes > o.capacity_bytes
            || o.sampled_at_ms < 0
        {
            return Err(ResourceError::Invalid);
        }
        let age = now_ms
            .checked_sub(o.sampled_at_ms)
            .ok_or(ResourceError::Invalid)?;
        if age < 0 || age > ttl_ms {
            return Err(ResourceError::StaleObservation);
        }
    }
    if known != observed {
        return Err(ResourceError::UnknownDomain);
    }
    let mut credited = BTreeSet::new();
    for floor in resident_floors {
        let existing = snapshot
            .owners
            .get(&floor.owner)
            .ok_or(ResourceError::Invalid)?;
        let observation = observations
            .iter()
            .find(|o| o.domain == floor.domain)
            .ok_or(ResourceError::UnknownDomain)?;
        if !credited.insert((&floor.owner, &floor.domain))
            || floor.bytes < 0
            || floor.bytes > amount(existing, &floor.domain).0
            || floor.sampled_at_ms != observation.sampled_at_ms
        {
            return Err(ResourceError::Invalid);
        }
    }
    Ok(())
}

fn validate_domains<'a>(
    footprints: impl Iterator<Item = &'a PhaseFootprint>,
    limits: &[MemoryLimit],
) -> Result<(), ResourceError> {
    for f in footprints {
        if f.allocations
            .iter()
            .any(|a| !limits.iter().any(|l| l.domain == a.domain))
        {
            return Err(ResourceError::UnknownDomain);
        }
    }
    Ok(())
}

fn validate_resident_total(
    observation: &MemoryObservation,
    floors: &[ResidentFloor],
) -> Result<(), ResourceError> {
    let total = floors
        .iter()
        .filter(|f| f.domain == observation.domain)
        .try_fold(0, |sum, f| add(sum, f.bytes))?;
    if total > observation.capacity_bytes - observation.available_bytes {
        return Err(ResourceError::Invalid);
    }
    Ok(())
}

/// Validates unchanged evidence before a forecast can remove an owner or credit bytes.
pub fn validate_admission_context(
    snapshot: &LedgerSnapshot,
    context: AdmissionContext<'_>,
) -> Result<(), ResourceError> {
    validate_context(snapshot, context)?;
    validate_domains(snapshot.owners.values(), context.limits)?;
    for observation in context.observations {
        validate_resident_total(observation, context.resident_floors)?;
    }
    Ok(())
}

pub fn admit_phase(
    snapshot: &LedgerSnapshot,
    owner: &str,
    next: &PhaseFootprint,
    context: AdmissionContext<'_>,
) -> Result<(), ResourceError> {
    if owner.is_empty() || context.ttl_ms <= 0 || context.now_ms < 0 || context.limits.is_empty() {
        return Err(ResourceError::Invalid);
    }
    validate_footprint(next)?;
    validate_context(snapshot, context)?;
    validate_domains(
        snapshot.owners.values().chain(std::iter::once(next)),
        context.limits,
    )?;
    let AdmissionContext {
        observations,
        resident_floors,
        limits,
        max_parked,
        ..
    } = context;
    let others = snapshot
        .owners
        .iter()
        .filter(|(id, _)| id.as_str() != owner);
    if others
        .clone()
        .any(|(_, f)| claims_conflict(&f.devices, &next.devices))
    {
        return Err(ResourceError::DeviceConflict);
    }
    let parked_count = others
        .clone()
        .filter(|(_, f)| f.phase == ResourcePhase::Parked)
        .count()
        + usize::from(next.phase == ResourcePhase::Parked);
    if parked_count > max_parked {
        return Err(ResourceError::CategoryLimit);
    }
    for l in limits {
        let o = observations
            .iter()
            .find(|o| o.domain == l.domain)
            .ok_or(ResourceError::UnknownDomain)?;
        let (candidate, candidate_kv) = amount(next, &l.domain);
        let floor = |id: &str| {
            resident_floors
                .iter()
                .find(|f| f.owner == id && f.domain == l.domain)
                .map(|f| f.bytes)
                .unwrap_or(0)
        };
        validate_resident_total(o, resident_floors)?;
        let own = candidate
            .checked_sub(floor(owner))
            .ok_or(ResourceError::Invalid)?
            .max(0);
        let mut remaining = own;
        let mut total = candidate;
        let mut kv = candidate_kv;
        let mut parked = if next.phase == ResourcePhase::Parked {
            candidate
        } else {
            0
        };
        for (id, f) in others.clone() {
            let (bytes, host_kv) = amount(f, &l.domain);
            remaining = add(
                remaining,
                bytes
                    .checked_sub(floor(id))
                    .ok_or(ResourceError::Invalid)?
                    .max(0),
            )?;
            total = add(total, bytes)?;
            kv = add(kv, host_kv)?;
            if f.phase == ResourcePhase::Parked {
                parked = add(parked, bytes)?;
            }
        }
        let ceiling = o
            .capacity_bytes
            .checked_sub(l.free_reserve_bytes)
            .ok_or(ResourceError::Invalid)?;
        if total > l.managed_bytes || total > ceiling {
            return Err(ResourceError::Insufficient);
        }
        if l.host_kv_bytes.is_some_and(|x| kv > x) || l.parked_bytes.is_some_and(|x| parked > x) {
            return Err(ResourceError::CategoryLimit);
        }
        // Found live on a 16 GB discrete GPU: a candidate that adds nothing
        // on this domain beyond what its own processes hold (a wake releasing
        // its host copy) is not judged on free memory. Refusing it cannot
        // restore the reserve; it only held the wake until its deadline.
        if own > 0
            && o.available_bytes
                .checked_sub(remaining)
                .ok_or(ResourceError::Invalid)?
                < l.free_reserve_bytes
        {
            return Err(ResourceError::Insufficient);
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservationProposal {
    expected_epoch: u64,
    owner: String,
    replacement: PhaseFootprint,
    valid_until_ms: i64,
}

impl ReservationProposal {
    pub fn expected_epoch(&self) -> u64 {
        self.expected_epoch
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn replacement(&self) -> &PhaseFootprint {
        &self.replacement
    }

    pub fn valid_until_ms(&self) -> i64 {
        self.valid_until_ms
    }
}

pub fn propose_phase(
    snapshot: &LedgerSnapshot,
    owner: &str,
    next: &PhaseFootprint,
    context: AdmissionContext<'_>,
) -> Result<ReservationProposal, ResourceError> {
    admit_phase(snapshot, owner, next, context)?;
    let earliest = context
        .observations
        .iter()
        .map(|o| o.sampled_at_ms)
        .min()
        .ok_or(ResourceError::UnknownDomain)?;
    let valid_until_ms = earliest
        .checked_add(context.ttl_ms)
        .ok_or(ResourceError::Invalid)?;
    Ok(ReservationProposal {
        expected_epoch: snapshot.epoch,
        owner: owner.into(),
        replacement: next.clone(),
        valid_until_ms,
    })
}

/// Applies a proposal to a pure hypothetical ledger state.
///
/// This helper performs no durable compare-and-swap, deployment fencing, physical
/// release, or engine authorization. The coordinator must supply those checks before
/// any real resource effect.
pub fn apply_proposal_to_snapshot(
    snapshot: &LedgerSnapshot,
    proposal: &ReservationProposal,
    now_ms: i64,
) -> Result<LedgerSnapshot, ResourceError> {
    if snapshot.epoch != proposal.expected_epoch {
        return Err(ResourceError::StaleEpoch);
    }
    if now_ms < 0 || now_ms > proposal.valid_until_ms {
        return Err(ResourceError::StaleObservation);
    }
    let mut next = snapshot.clone();
    next.epoch = next.epoch.checked_add(1).ok_or(ResourceError::Invalid)?;
    next.owners
        .insert(proposal.owner.clone(), proposal.replacement.clone());
    Ok(next)
}
