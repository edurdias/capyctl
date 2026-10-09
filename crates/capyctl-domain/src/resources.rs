use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
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
    /// SPEC §7.2 (found live 2026-10-09: a 109.25 GiB cold charge under a
    /// 110 GiB managed limit was refused with no figures at all): the
    /// charge fits the managed limit, but the memory the host has available
    /// now, less the charge, would not leave the free reserve. Carries the
    /// figures the operator needs.
    #[error("{0}")]
    InsufficientAvailable(Box<AvailableShortfall>),
    #[error("resource category limit exceeded")]
    CategoryLimit,
    #[error("stale ledger epoch")]
    StaleEpoch,
}

/// SPEC §7.2: why the free-memory check refused a charge on one domain, in
/// bytes: what the host has available now, what the admission must find
/// there (the candidate's own charge and the charges of other starts not
/// yet resident), the free memory that must remain (the free reserve, less
/// what a device domain's reserve already absorbs, ADR 0019 §2) and how much
/// is missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableShortfall {
    /// The ledger key of the domain (a host-scoped key on an enrolled host).
    pub domain: String,
    /// Whether the domain is a GPU's memory (ADR 0019).
    pub device: bool,
    pub available_bytes: i64,
    /// The candidate's own charge beyond what its processes already hold.
    pub charge_bytes: i64,
    /// Other owners' charges on the domain that are not resident yet.
    pub pending_bytes: i64,
    pub free_reserve_bytes: i64,
    pub short_bytes: i64,
}

impl std::fmt::Display for AvailableShortfall {
    /// In the closed code status classifies (`insufficient_memory` or
    /// `insufficient_device_memory`) and capacity_blocked's wording: e.g.
    /// `insufficient_memory: needs 109.2 GiB of system memory, 118.2 GiB
    /// available and a 11.0 GiB free reserve to keep, 2.0 GiB short`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let code = if self.device {
            "insufficient_device_memory"
        } else {
            "insufficient_memory"
        };
        write!(
            f,
            "{code}: needs {} of {} memory",
            gib(self.charge_bytes),
            local_domain(&self.domain)
        )?;
        if self.pending_bytes > 0 {
            write!(
                f,
                " beside {} charged to starts not yet resident",
                gib(self.pending_bytes)
            )?;
        }
        write!(
            f,
            ", {} available and a {} free reserve to keep, {} short",
            gib(self.available_bytes),
            gib(self.free_reserve_bytes),
            gib(self.short_bytes)
        )
    }
}

/// Bytes shown to the operator, in GiB with one decimal.
pub fn gib(bytes: i64) -> String {
    format!("{:.1} GiB", bytes.max(0) as f64 / (1u64 << 30) as f64)
}

/// A domain's own name: an enrolled host's ledger key
/// (`host/<n>:<host>/domain/<id>`) shown as its local id.
fn local_domain(key: &str) -> &str {
    match key
        .strip_prefix("host/")
        .and_then(|k| k.rsplit_once("/domain/"))
    {
        Some((_, local)) => local,
        None => key,
    }
}

/// SPEC §7.2 (found live 2026-10-09): a deployment is admitted against the
/// memory a host has available now, not its total, and must leave the free
/// reserve. When the domain's managed limit plus that reserve exceeds what is
/// available now (plus what deployments already charged there hold), a
/// deployment sized near the limit is refused until memory is freed, though
/// the limits fit the total. The warning that says so with its figures, or
/// `None`. A warning only: nothing is refused for it.
pub fn headroom_warning(
    limit: &MemoryLimit,
    observation: &MemoryObservation,
    charged_bytes: i64,
) -> Option<String> {
    // ADR 0019 §2: a device domain keeps only the part of its reserve that
    // unaccounted memory does not already take; nothing is credited as held.
    let reserve = limit.required_free(observation.capacity_bytes, observation.available_bytes, 0);
    let needed = limit.managed_bytes.saturating_add(reserve);
    let covered = observation
        .available_bytes
        .saturating_add(charged_bytes.max(0));
    if needed <= covered {
        return None;
    }
    let held = if charged_bytes > 0 {
        format!(
            " (plus {} charged to deployments there)",
            gib(charged_bytes)
        )
    } else {
        String::new()
    };
    Some(format!(
        "{} memory has {} available{held}, less than its {} managed limit plus its {} free \
         reserve ({}): a deployment near the limit cannot be admitted until {} more is free",
        local_domain(&limit.domain),
        gib(observation.available_bytes),
        gib(limit.managed_bytes),
        gib(reserve),
        gib(needed),
        gib(needed - covered)
    ))
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
    /// ADR 0019 §2 (found live on a 16 GB laptop GPU, 2026-10-03): on a
    /// `device` domain the free reserve absorbs memory the card holds outside
    /// CapyCTL's accounting (the driver's own reservation, a display server).
    /// Only the part of the reserve that this memory does not already take must
    /// stay free; see [`MemoryLimit::required_free`]. Always `false` on host
    /// memory, where the whole reserve stays free for the operating system.
    pub reserve_absorbs_unmanaged: bool,
    pub host_kv_bytes: Option<i64>,
    pub parked_bytes: Option<i64>,
}

impl MemoryLimit {
    /// The free memory that must remain after admission on this domain,
    /// given its observation and the memory CapyCTL's own processes were
    /// sampled holding there (`held`, a verified lower bound).
    ///
    /// ADR 0019 §2: the ledger keeps every charge within `managed_bytes`
    /// (capacity minus the reserve), so the reserve is already outside every
    /// charge. On a device domain the memory nobody accounts for (capacity
    /// minus available minus `held`) sits in that reserve; requiring the whole
    /// reserve free on top of it counted it twice, and a deployment sized to
    /// the managed limit could never start (a 16 GB card loses about 0.35 GiB
    /// to the driver's own reservation before any process runs). Unattributed
    /// engine memory counts as unaccounted, which only lowers the requirement
    /// to the physical fit, never below it.
    pub fn required_free(&self, capacity: i64, available: i64, held: i64) -> i64 {
        if !self.reserve_absorbs_unmanaged {
            return self.free_reserve_bytes;
        }
        absorbing_reserve(self.free_reserve_bytes, capacity, available, held)
    }
}

/// ADR 0019 §2: the part of a device domain's `reserve` that must stay free
/// when the card has `available` of `capacity` bytes free and CapyCTL's own
/// processes hold `held` bytes of the rest: the reserve less what nobody
/// accounts for, never negative.
pub fn absorbing_reserve(reserve: i64, capacity: i64, available: i64, held: i64) -> i64 {
    let unaccounted = capacity
        .saturating_sub(available)
        .saturating_sub(held)
        .max(0);
    reserve.saturating_sub(unaccounted).max(0)
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

// SPEC §7.2: the free-memory refusal names its figures in capacity_blocked's
// wording, under the closed code status classifies.
#[test]
fn the_free_memory_refusal_names_its_figures() {
    let refusal = ResourceError::InsufficientAvailable(Box::new(AvailableShortfall {
        domain: "system".into(),
        device: false,
        available_bytes: 118 << 30,
        charge_bytes: 109 << 30,
        pending_bytes: 0,
        free_reserve_bytes: 11 << 30,
        short_bytes: 2 << 30,
    }));
    assert_eq!(
        refusal.to_string(),
        "insufficient_memory: needs 109.0 GiB of system memory, 118.0 GiB available and a \
         11.0 GiB free reserve to keep, 2.0 GiB short"
    );
    let ResourceError::InsufficientAvailable(mut shortfall) = refusal else {
        unreachable!()
    };
    shortfall.domain = "host/4:h-01/domain/gpu0".into();
    shortfall.device = true;
    shortfall.pending_bytes = 3 << 30;
    assert_eq!(
        shortfall.to_string(),
        "insufficient_device_memory: needs 109.0 GiB of gpu0 memory beside 3.0 GiB charged to \
         starts not yet resident, 118.0 GiB available and a 11.0 GiB free reserve to keep, \
         2.0 GiB short"
    );
}

// SPEC §7.2: the start-time warning appears only when the managed limit plus
// the free reserve exceeds the memory available now (with what deployments
// already charged there hold); the total alone never decides it.
#[test]
fn the_headroom_warning_appears_only_when_available_memory_falls_short() {
    const GIB: i64 = 1 << 30;
    let limit = MemoryLimit {
        domain: "system".into(),
        managed_bytes: 110 * GIB,
        free_reserve_bytes: 11 * GIB,
        reserve_absorbs_unmanaged: false,
        host_kv_bytes: None,
        parked_bytes: None,
    };
    let observed = |available: i64| MemoryObservation {
        domain: "system".into(),
        capacity_bytes: 122 * GIB,
        available_bytes: available,
        sampled_at_ms: 1,
    };
    let warning = headroom_warning(&limit, &observed(118 * GIB), 0).expect("short");
    for figure in [
        "118.0 GiB",
        "110.0 GiB",
        "11.0 GiB",
        "121.0 GiB",
        "3.0 GiB more",
    ] {
        assert!(warning.contains(figure), "{figure}: {warning}");
    }
    assert!(warning.starts_with("system memory"), "{warning}");
    // Enough available, or the shortfall held by deployments already charged.
    assert_eq!(headroom_warning(&limit, &observed(121 * GIB), 0), None);
    assert_eq!(
        headroom_warning(&limit, &observed(118 * GIB), 3 * GIB),
        None
    );
    let held = headroom_warning(&limit, &observed(100 * GIB), 20 * GIB / 2).unwrap();
    assert!(held.contains("plus 10.0 GiB charged"), "{held}");
    assert!(held.contains("11.0 GiB more"), "{held}");
    // ADR 0019 §2: a device domain at idle, the driver's own memory inside
    // its reserve, can hold a deployment at its limit: no warning.
    let device = MemoryLimit {
        domain: "gpu0".into(),
        managed_bytes: 14 * GIB,
        free_reserve_bytes: 2 * GIB,
        reserve_absorbs_unmanaged: true,
        ..limit
    };
    let card = MemoryObservation {
        domain: "gpu0".into(),
        capacity_bytes: 16 * GIB,
        available_bytes: 15 * GIB,
        sampled_at_ms: 1,
    };
    assert_eq!(headroom_warning(&device, &card, 0), None);
}
