use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ResourceError {
    #[error("invalid resource contract")]
    Invalid,
    #[error("unknown physical domain")]
    UnknownDomain,
    #[error("stale resource observation")]
    StaleObservation,
    #[error("device assignment conflict")]
    DeviceConflict,
    #[error("insufficient resources")]
    Insufficient,
    #[error("resource category limit exceeded")]
    CategoryLimit,
    #[error("stale ledger epoch")]
    StaleEpoch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryObservation {
    pub domain: String,
    pub capacity_bytes: i64,
    pub available_bytes: i64,
    pub sampled_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Qualified lower-bound attribution of resident bytes to one owner and domain.
///
/// The attribution must come from the same coherent availability sample and current
/// runtime identity as the corresponding [`MemoryObservation`]. It is neither a
/// reservation nor credit inferred from a global availability delta. Use zero credit
/// when qualified attribution is unavailable. Numeric validation of these fields
/// cannot establish the attribution itself.
pub struct ResidentFloor {
    pub owner: String,
    pub domain: String,
    pub bytes: i64,
    pub sampled_at_ms: i64,
}

/// ADR 0007: the memory one process holds, as its host sampled it together
/// with the availability it reports (GPU memory the driver attributes to the
/// process plus its anonymous resident pages). Bound to the process identity
/// (pid, boot id and start ticks), never to a deployment: the lifecycle
/// authority attributes it to an owner only by matching that identity against
/// the runtime it recorded, and credits it only as a lower bound
/// ([`ResidentFloor`]). A reading source cannot grant credit by itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessResident {
    pub pid: u32,
    pub boot_id: String,
    pub start_ticks: u64,
    /// `device_bytes + host_bytes`: the credit on a unified domain, where
    /// both come from one pool, and the only figure an older peer reports.
    pub bytes: i64,
    /// ADR 0019: GPU memory the driver attributes to the process, credited
    /// against a `device` domain. Zero when the reporter did not split it.
    pub device_bytes: i64,
    /// ADR 0019: anonymous resident pages, credited against a `distinct`
    /// system domain. Zero when the reporter did not split it.
    pub host_bytes: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryLimit {
    pub domain: String,
    pub managed_bytes: i64,
    pub free_reserve_bytes: i64,
    pub host_kv_bytes: Option<i64>,
    pub parked_bytes: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sharing {
    Shared,
    Exclusive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceClaim {
    pub device: String,
    pub sharing: Sharing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allocation {
    pub domain: String,
    pub bytes: i64,
    pub host_kv_bytes: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourcePhase {
    Cold,
    Ready,
    Parking,
    Parked,
    Wake,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseFootprint {
    pub phase: ResourcePhase,
    pub allocations: Vec<Allocation>,
    pub devices: Vec<DeviceClaim>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipeFootprints {
    pub cold: PhaseFootprint,
    pub ready: PhaseFootprint,
    pub parking: PhaseFootprint,
    pub parked: PhaseFootprint,
    pub wake: PhaseFootprint,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerSnapshot {
    pub epoch: u64,
    pub owners: BTreeMap<String, PhaseFootprint>,
}

pub fn validate_footprint(f: &PhaseFootprint) -> Result<(), ResourceError> {
    let mut domains = BTreeSet::new();
    let mut devices = BTreeSet::new();
    if f.allocations.is_empty() {
        return Err(ResourceError::Invalid);
    }
    for a in &f.allocations {
        if a.domain.is_empty()
            || !domains.insert(&a.domain)
            || a.bytes < 0
            || a.host_kv_bytes < 0
            || a.host_kv_bytes > a.bytes
        {
            return Err(ResourceError::Invalid);
        }
    }
    for d in &f.devices {
        if d.device.is_empty() || !devices.insert(&d.device) {
            return Err(ResourceError::Invalid);
        }
    }
    if f.phase == ResourcePhase::Parked && !f.devices.is_empty() {
        return Err(ResourceError::Invalid);
    }
    Ok(())
}

pub fn claims_conflict(a: &[DeviceClaim], b: &[DeviceClaim]) -> bool {
    a.iter().any(|x| {
        b.iter().any(|y| {
            x.device == y.device
                && (x.sharing == Sharing::Exclusive || y.sharing == Sharing::Exclusive)
        })
    })
}

pub fn validate_recipe(r: &RecipeFootprints) -> Result<(), ResourceError> {
    for (f, expected) in [
        (&r.cold, ResourcePhase::Cold),
        (&r.ready, ResourcePhase::Ready),
        (&r.parking, ResourcePhase::Parking),
        (&r.parked, ResourcePhase::Parked),
        (&r.wake, ResourcePhase::Wake),
    ] {
        validate_footprint(f)?;
        if f.phase != expected {
            return Err(ResourceError::Invalid);
        }
    }
    let domain_set = |f: &PhaseFootprint| {
        f.allocations
            .iter()
            .map(|a| a.domain.clone())
            .collect::<BTreeSet<_>>()
    };
    let domains = domain_set(&r.ready);
    for f in [&r.cold, &r.parking, &r.parked, &r.wake] {
        if domain_set(f) != domains {
            return Err(ResourceError::Invalid);
        }
    }
    for (peak, base) in [
        (&r.cold, &r.ready),
        (&r.parking, &r.ready),
        (&r.parking, &r.parked),
        (&r.wake, &r.parked),
        (&r.wake, &r.ready),
    ] {
        for b in &base.allocations {
            let p = peak
                .allocations
                .iter()
                .find(|p| p.domain == b.domain)
                .ok_or(ResourceError::Invalid)?;
            if p.bytes < b.bytes || p.host_kv_bytes < b.host_kv_bytes {
                return Err(ResourceError::Invalid);
            }
        }
    }
    Ok(())
}

#[test]
fn phase_names_include_transient_parking() {
    assert_ne!(ResourcePhase::Parking, ResourcePhase::Parked);
    assert_ne!(ResourcePhase::Cold, ResourcePhase::Wake);
}
