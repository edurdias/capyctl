use std::collections::HashSet;

use mllm_domain::OwnerAccountId;
use thiserror::Error;

use crate::ledger::{
    charged_bytes, index_by_domain, Category, Domain, DomainKind, HostLimits, Phase, Reservation,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum BlockReason {
    #[error("insufficient resources")]
    InsufficientResources,
    #[error("unreconciled ownership")]
    UnreconciledOwnership,
    #[error("device conflict")]
    DeviceConflict,
    #[error("category sub-limit exceeded")]
    CategoryLimit,
    #[error("unknown topology")]
    UnknownTopology,
    #[error("no safe estimate")]
    NoSafeEstimate,
    #[error("stale observation")]
    StaleObservation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub owner: OwnerAccountId,
    pub domain: String,
    pub activation_peak: i64,
    /// Some => replace-don't-stack against the candidate's own Parked reservation.
    pub parked_budget: Option<i64>,
    pub category: Option<Category>,
    pub devices: Vec<String>,
}

pub fn admit(
    domains: &[Domain],
    reservations: &[Reservation],
    candidate: &Candidate,
    limits: &HostLimits,
) -> Result<(), BlockReason> {
    // Check order is driven by the F0 tests: capacity/sub-limit/transition checks
    // run before topology lookup (several tests admit against undeclared domains).
    let ledger = index_by_domain(reservations);

    // Device-set exclusivity: the candidate's devices must be disjoint from all
    // other owners' exclusive device sets.
    let other_devices: HashSet<&str> = reservations
        .iter()
        .filter(|r| r.owner != candidate.owner)
        .flat_map(|r| r.devices.iter().map(String::as_str))
        .collect();
    if candidate
        .devices
        .iter()
        .any(|d| other_devices.contains(d.as_str()))
    {
        return Err(BlockReason::DeviceConflict);
    }

    // Category sub-limits: sum only tagged owners' bytes plus the candidate's own
    // contribution when the candidate carries the same tag.
    let mut host_kv = 0i64;
    let mut parked = 0i64;
    for r in reservations.iter().filter(|r| r.domain == candidate.domain) {
        match r.category {
            Some(Category::HostKv) => host_kv = host_kv.saturating_add(r.bytes),
            Some(Category::ParkedResidue) => parked = parked.saturating_add(r.bytes),
            _ => {}
        }
    }
    match candidate.category {
        Some(Category::HostKv) => host_kv = host_kv.saturating_add(candidate.activation_peak),
        Some(Category::ParkedResidue) => {
            parked =
                parked.saturating_add(candidate.parked_budget.unwrap_or(candidate.activation_peak))
        }
        _ => {}
    }
    if limits.host_kv_limit.is_some_and(|limit| host_kv > limit)
        || limits.parked_limit.is_some_and(|limit| parked > limit)
    {
        return Err(BlockReason::CategoryLimit);
    }

    // Transition-peak validation: activation_peak must cover the parked residue
    // that stays physically resident during the wake transition (design §5.2).
    if candidate
        .parked_budget
        .is_some_and(|budget| candidate.activation_peak < budget)
    {
        return Err(BlockReason::InsufficientResources);
    }

    // Union charging: all owners' bytes on the domain, including the candidate's
    // own reservations. When the candidate itself holds a Parked reservation, its
    // budget replaces rather than stacks (replace-don't-stack).
    let charged = charged_bytes(&ledger, &candidate.domain).saturating_add(rollup_bytes(
        domains,
        reservations,
        &candidate.domain,
    ));
    let holds_parked = ledger
        .get(&candidate.domain)
        .and_then(|owners| owners.get(&candidate.owner))
        .is_some_and(|rs| rs.iter().any(|r| r.phase == Phase::Parked));
    let delta = candidate.activation_peak
        - if holds_parked {
            candidate.parked_budget.unwrap_or(0)
        } else {
            0
        };
    if charged.saturating_add(delta) > limits.managed_limit {
        return Err(BlockReason::InsufficientResources);
    }

    // Topology lookup and observation freshness.
    let domain = domains
        .iter()
        .find(|d| d.id == candidate.domain)
        .ok_or(BlockReason::UnknownTopology)?;
    let observed = domain.observed_bytes.ok_or(BlockReason::UnknownTopology)?;
    if limits.now_unix - domain.observed_at_unix > limits.observation_ttl_secs as i64 {
        return Err(BlockReason::StaleObservation);
    }

    // Free reserve: the host must keep its protected headroom.
    if observed < limits.free_reserve {
        return Err(BlockReason::InsufficientResources);
    }

    Ok(())
}

/// Unified-memory roll-up (design §5.1): allocations referencing domain labels
/// that are not declared domains are carved out of the host's declared system
/// domain, so their bytes charge against it too. Disjoint device pools still
/// share system RAM.
fn rollup_bytes(domains: &[Domain], reservations: &[Reservation], target: &str) -> i64 {
    let target_declared = domains
        .iter()
        .any(|d| d.id == target && d.kind == DomainKind::System);
    if !target_declared {
        return 0;
    }
    reservations
        .iter()
        .filter(|r| r.domain != target && !domains.iter().any(|d| d.id == r.domain))
        .fold(0i64, |acc, r| acc.saturating_add(r.bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sys(observed: i64, age: i64) -> Domain {
        Domain {
            id: "system".into(),
            kind: DomainKind::System,
            observed_bytes: Some(observed),
            observed_at_unix: age,
        }
    }
    fn res(owner: &str, bytes: i64) -> Reservation {
        Reservation {
            owner: OwnerAccountId(owner.into()),
            domain: "system".into(),
            bytes,
            phase: Phase::Ready,
            category: None,
            devices: vec![],
        }
    }
    const GI_B: i64 = 1024 * 1024 * 1024;
    fn cand_min() -> Candidate {
        Candidate {
            owner: OwnerAccountId("C".into()),
            domain: "system".into(),
            activation_peak: GI_B,
            parked_budget: None,
            category: None,
            devices: vec![],
        }
    }

    #[test]
    fn t26_unified_memory_charged_once() {
        // Spark: one system domain; a CPU-side weight copy is NOT extra capacity.
        let d = sys(128 * GI_B, 0);
        let limits = HostLimits {
            managed_limit: 96 * GI_B,
            free_reserve: 12 * GI_B,
            host_kv_limit: None,
            parked_limit: None,
            observation_ttl_secs: 60,
            now_unix: 0,
        };
        let existing = vec![res("A", 48 * GI_B)]; // A owns 48 GiB once
        let cand = Candidate {
            owner: OwnerAccountId("B".into()),
            domain: "system".into(),
            activation_peak: 48 * GI_B,
            parked_budget: None,
            category: None,
            devices: vec![],
        };
        assert!(admit(std::slice::from_ref(&d), &existing, &cand, &limits).is_ok()); // 96 ≤ 96
        let cand2 = Candidate {
            activation_peak: 49 * GI_B,
            ..cand
        };
        assert!(matches!(
            admit(&[d], &existing, &cand2, &limits),
            Err(BlockReason::InsufficientResources)
        )); // 97 > 96
    }

    #[test]
    fn t27_disjoint_devices_still_share_system_ram() {
        // Disjoint exclusive device pools do not create extra system-RAM capacity.
        let limits = HostLimits {
            managed_limit: 56 * GI_B,
            free_reserve: 8 * GI_B,
            host_kv_limit: None,
            parked_limit: None,
            observation_ttl_secs: 60,
            now_unix: 0,
        };
        let g0 = Reservation {
            owner: OwnerAccountId("A".into()),
            domain: "gpu:0".into(),
            bytes: 24 * GI_B,
            phase: Phase::Ready,
            category: None,
            devices: vec!["gpu:0".into()],
        };
        let g1 = Reservation {
            owner: OwnerAccountId("B".into()),
            domain: "gpu:1".into(),
            bytes: 24 * GI_B,
            phase: Phase::Ready,
            category: None,
            devices: vec!["gpu:1".into()],
        };
        let cand = Candidate {
            owner: OwnerAccountId("C".into()),
            domain: "system".into(),
            activation_peak: 20 * GI_B,
            parked_budget: None,
            category: None,
            devices: vec!["gpu:2".into()],
        };
        // Device sets are disjoint; the system domain still enforces its limit.
        assert!(matches!(
            admit(&[sys(64 * GI_B, 0)], &[g0, g1], &cand, &limits),
            Err(BlockReason::InsufficientResources)
        ));
    }

    #[test]
    fn t24_shape_sublimit_blocks_on_retained_host_kv() {
        // host_kv owners at 9 + 8 > 16 GiB: B is blocked, not shrunk.
        let limits = HostLimits {
            managed_limit: 96 * GI_B,
            free_reserve: 12 * GI_B,
            host_kv_limit: Some(16 * GI_B),
            parked_limit: None,
            observation_ttl_secs: 60,
            now_unix: 0,
        };
        let kv_a = Reservation {
            owner: OwnerAccountId("A-kv".into()),
            domain: "system".into(),
            bytes: 9 * GI_B,
            phase: Phase::Ready,
            category: Some(Category::HostKv),
            devices: vec![],
        };
        let cand = Candidate {
            owner: OwnerAccountId("B".into()),
            domain: "system".into(),
            activation_peak: 8 * GI_B,
            parked_budget: None,
            category: Some(Category::HostKv),
            devices: vec![],
        };
        assert!(matches!(
            admit(&[], &[kv_a], &cand, &limits),
            Err(BlockReason::CategoryLimit)
        ));
    }

    #[test]
    fn replace_dont_stack_includes_candidate_in_charged() {
        // C parked at 2 GiB; activation peak 48 GiB; others hold 96 GiB; limit 96+8 = safe only
        // if the sum includes C's own parked bytes then swaps them out.
        let limits = HostLimits {
            managed_limit: 100 * GI_B,
            free_reserve: 0,
            host_kv_limit: None,
            parked_limit: None,
            observation_ttl_secs: 60,
            now_unix: 0,
        };
        let others = vec![res("X", 96 * GI_B)];
        let c_parked = Reservation {
            owner: OwnerAccountId("C".into()),
            domain: "system".into(),
            bytes: 2 * GI_B,
            phase: Phase::Parked,
            category: None,
            devices: vec![],
        };
        let cand = Candidate {
            owner: OwnerAccountId("C".into()),
            domain: "system".into(),
            activation_peak: 6 * GI_B,
            parked_budget: Some(2 * GI_B),
            category: None,
            devices: vec![],
        };
        // charged(incl C) = 98; delta = 6-2 = 4; total 102 > 100 → blocked.
        let all: Vec<Reservation> = [c_parked].into_iter().chain(others).collect();
        assert!(matches!(
            admit(&[], &all, &cand, &limits),
            Err(BlockReason::InsufficientResources)
        ));
    }

    #[test]
    fn hostile_huge_values_block_instead_of_overflowing() {
        // i64::MAX/2-sized reservations must saturate, not wrap into a
        // spurious admission grant (or panic in debug builds).
        let limits = HostLimits {
            managed_limit: 96 * GI_B,
            free_reserve: 12 * GI_B,
            host_kv_limit: None,
            parked_limit: None,
            observation_ttl_secs: 60,
            now_unix: 0,
        };
        let huge = i64::MAX / 2;
        let existing = vec![res("A", huge), res("B", huge)];
        let cand = Candidate {
            activation_peak: huge,
            ..cand_min()
        };
        assert!(matches!(
            admit(&[sys(128 * GI_B, 0)], &existing, &cand, &limits),
            Err(BlockReason::InsufficientResources)
        ));
    }

    #[test]
    fn stale_observation_blocks_admission() {
        let d = sys(128 * GI_B, /*observed at*/ 3600);
        let limits = HostLimits {
            managed_limit: 96 * GI_B,
            free_reserve: 12 * GI_B,
            host_kv_limit: None,
            parked_limit: None,
            observation_ttl_secs: 60,
            now_unix: 7200,
        };
        assert!(matches!(
            admit(&[d], &[], &cand_min(), &limits),
            Err(BlockReason::StaleObservation)
        ));
    }

    #[test]
    fn transition_peak_must_cover_parked_residue() {
        // activation_peak 4 GiB cannot replace a parked 8 GiB residue → reject.
        let limits = HostLimits {
            managed_limit: 100 * GI_B,
            free_reserve: 0,
            host_kv_limit: None,
            parked_limit: None,
            observation_ttl_secs: 60,
            now_unix: 0,
        };
        let c_parked = Reservation {
            owner: OwnerAccountId("C".into()),
            domain: "system".into(),
            bytes: 8 * GI_B,
            phase: Phase::Parked,
            category: None,
            devices: vec![],
        };
        let cand = Candidate {
            owner: OwnerAccountId("C".into()),
            domain: "system".into(),
            activation_peak: 4 * GI_B,
            parked_budget: Some(8 * GI_B),
            category: None,
            devices: vec![],
        };
        assert!(matches!(
            admit(&[], &[c_parked], &cand, &limits),
            Err(BlockReason::InsufficientResources)
        ));
    }
}
