//! ADR 0013 §4: choosing the host for one instance when it must activate.
//!
//! Pure and deterministic: the caller supplies every allowed host that resolved
//! the revision, with its eligibility, this deployment's instances already
//! there, the instance's activation footprint as resolved on that host, the
//! host's limits and the ledger as that host's judgement sees it (every owner
//! whose charges fall on this host, including starts accepted but not yet
//! armed). Nothing here reserves anything: the chosen host's fit is judged
//! again, with fresh observations and an epoch compare-and-swap, when the
//! activation arms (ADR 0007). A host that fits nowhere is refused with its
//! reason, never placed over budget (ADR 0013 §4 step 4).

use std::cmp::Ordering;

use mllm_domain::resources::{
    claims_conflict, validate_footprint, LedgerSnapshot, MemoryLimit, PhaseFootprint, ResourcePhase,
};

/// ADR 0013 §4 step 3: how candidate hosts are ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// Fewest instances of this deployment, then most remaining headroom.
    Spread,
    /// Most instances of this deployment, then least headroom that fits.
    Pack,
}

/// One allowed host the revision resolved on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostCandidate {
    pub host_id: String,
    /// W12: a live reconciled session, not revoked, not draining, an approved
    /// configuration and a matching profile build.
    pub eligible: bool,
    /// Instances of this deployment already on the host (running or placed
    /// and starting), not counting the one being placed.
    pub instances_here: u32,
    /// The instance's activation footprint as resolved on this host.
    pub footprint: PhaseFootprint,
    pub limits: Vec<MemoryLimit>,
    /// The ledger as this host's admission sees it, without the instance
    /// being placed.
    pub ledger: LedgerSnapshot,
    pub max_parked: usize,
    /// The host can run no further launch now, whatever the reservations
    /// say: an enrolled host whose agent holds one journal claim at a time
    /// runs another launch, or a host with per-launch claims (SPEC §§3.1,
    /// 7.3) already runs another instance of this deployment, whose commands
    /// its per-deployment generation fence would refuse.
    pub occupied: bool,
    /// Owner decision 2026-09-23: `footprint` is the host's whole managed
    /// limit, reserved for an unmeasured model whose startup estimate exceeds
    /// it. It starts only when no other owner holds a charge on the host.
    pub whole_host: bool,
}

/// Why a host cannot take the instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostRefusal {
    Ineligible,
    MaxPerHost,
    /// The host cannot take another launch (see `HostCandidate::occupied`).
    Occupied,
    /// An exclusive device claim conflicts with an existing one (SPEC §7.3).
    DeviceConflict,
    /// A managed, host-KV or parked limit would be exceeded.
    Insufficient,
    /// The footprint names a domain the host does not limit, or is malformed.
    Invalid,
    /// Owner decision 2026-09-23: a whole-host first start while another
    /// owner holds a charge on the host.
    RequiresEmptyHost,
}

impl HostRefusal {
    /// The closed diagnostic status shows.
    pub fn code(self) -> &'static str {
        match self {
            Self::Ineligible => "host_ineligible",
            Self::MaxPerHost => "max_per_host",
            Self::Occupied => "host_occupied",
            Self::DeviceConflict => "device_conflict",
            Self::Insufficient => "insufficient_capacity",
            Self::Invalid => "invalid_footprint",
            Self::RequiresEmptyHost => "startup_requires_empty_host",
        }
    }
}

/// The chosen host and the headroom the choice leaves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    pub host_id: String,
    pub headroom_bytes: i64,
}

/// No host can take the instance without releasing capacity. Every allowed
/// host is listed with its reason; the caller hands this to the switching
/// rules (W10) or reports it, and never places over budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unplaceable {
    pub refusals: Vec<(String, HostRefusal)>,
}

impl Unplaceable {
    /// The one closed diagnostic for status: the first host's reason when all
    /// agree, otherwise that no allowed host fits.
    pub fn code(&self) -> &'static str {
        match self.refusals.as_slice() {
            [] => "no_allowed_host",
            [(_, first), rest @ ..] if rest.iter().all(|(_, r)| r == first) => first.code(),
            _ => "no_host_fits",
        }
    }
}

fn amount(footprint: &PhaseFootprint, domain: &str) -> (i64, i64) {
    footprint
        .allocations
        .iter()
        .find(|a| a.domain == domain)
        .map(|a| (a.bytes, a.host_kv_bytes))
        .unwrap_or((0, 0))
}

/// ADR 0007 as far as reservations alone decide it: device conflicts, every
/// domain's managed, host-KV and parked limits, and `max_parked`. Returns the
/// bottleneck headroom (the smallest managed limit left over any domain the
/// host limits) when the footprint fits. Observed free memory is judged again
/// at arm, where fresh observations exist.
pub fn fits(
    ledger: &LedgerSnapshot,
    owner: &str,
    footprint: &PhaseFootprint,
    limits: &[MemoryLimit],
    max_parked: usize,
) -> Result<i64, HostRefusal> {
    validate_footprint(footprint).map_err(|_| HostRefusal::Invalid)?;
    if limits.is_empty()
        || footprint
            .allocations
            .iter()
            .any(|a| !limits.iter().any(|l| l.domain == a.domain))
    {
        return Err(HostRefusal::Invalid);
    }
    let others = ledger.owners.iter().filter(|(id, _)| id.as_str() != owner);
    if others
        .clone()
        .any(|(_, f)| claims_conflict(&f.devices, &footprint.devices))
    {
        return Err(HostRefusal::DeviceConflict);
    }
    let parked = others
        .clone()
        .filter(|(_, f)| f.phase == ResourcePhase::Parked)
        .count()
        + usize::from(footprint.phase == ResourcePhase::Parked);
    if parked > max_parked {
        return Err(HostRefusal::Insufficient);
    }
    let mut headroom = i64::MAX;
    for limit in limits {
        let (mut total, mut kv) = amount(footprint, &limit.domain);
        let mut parked_bytes = if footprint.phase == ResourcePhase::Parked {
            total
        } else {
            0
        };
        for (_, other) in others.clone() {
            let (bytes, host_kv) = amount(other, &limit.domain);
            total = total.checked_add(bytes).ok_or(HostRefusal::Invalid)?;
            kv = kv.checked_add(host_kv).ok_or(HostRefusal::Invalid)?;
            if other.phase == ResourcePhase::Parked {
                parked_bytes = parked_bytes
                    .checked_add(bytes)
                    .ok_or(HostRefusal::Invalid)?;
            }
        }
        if total > limit.managed_bytes
            || limit.host_kv_bytes.is_some_and(|max| kv > max)
            || limit.parked_bytes.is_some_and(|max| parked_bytes > max)
        {
            return Err(HostRefusal::Insufficient);
        }
        headroom = headroom.min(limit.managed_bytes - total);
    }
    Ok(headroom)
}

/// ADR 0013 §4: place one instance. `preferred` is the host a stopped
/// instance last ran on: it is chosen when it is eligible, under
/// `max_per_host` and fits, and otherwise ordinary ordering applies. Ties
/// break by host id, so the choice is deterministic.
pub fn place(
    candidates: &[HostCandidate],
    owner: &str,
    strategy: Strategy,
    max_per_host: Option<u32>,
    preferred: Option<&str>,
) -> Result<Placement, Unplaceable> {
    let mut fitting = Vec::new();
    let mut refusals = Vec::new();
    for candidate in candidates {
        let verdict = if !candidate.eligible {
            Err(HostRefusal::Ineligible)
        } else if max_per_host.is_some_and(|max| candidate.instances_here >= max) {
            Err(HostRefusal::MaxPerHost)
        } else if candidate.occupied {
            Err(HostRefusal::Occupied)
        } else if candidate.whole_host && candidate.ledger.owners.keys().any(|other| other != owner)
        {
            // Owner decision 2026-09-23: no other engine charge on the host.
            Err(HostRefusal::RequiresEmptyHost)
        } else {
            fits(
                &candidate.ledger,
                owner,
                &candidate.footprint,
                &candidate.limits,
                candidate.max_parked,
            )
        };
        match verdict {
            Ok(headroom) => fitting.push((candidate, headroom)),
            Err(refusal) => refusals.push((candidate.host_id.clone(), refusal)),
        }
    }
    refusals.sort_by(|a, b| a.0.cmp(&b.0));
    if let Some((candidate, headroom)) =
        preferred.and_then(|host| fitting.iter().find(|(c, _)| c.host_id == host))
    {
        return Ok(Placement {
            host_id: candidate.host_id.clone(),
            headroom_bytes: *headroom,
        });
    }
    fitting.sort_by(|(a, a_room), (b, b_room)| {
        let by_count = match strategy {
            Strategy::Spread => a.instances_here.cmp(&b.instances_here),
            Strategy::Pack => b.instances_here.cmp(&a.instances_here),
        };
        let by_room = match strategy {
            Strategy::Spread => b_room.cmp(a_room),
            Strategy::Pack => a_room.cmp(b_room),
        };
        by_count
            .then(by_room)
            .then_with(|| a.host_id.cmp(&b.host_id))
            .then(Ordering::Equal)
    });
    fitting
        .first()
        .map(|(candidate, headroom)| Placement {
            host_id: candidate.host_id.clone(),
            headroom_bytes: *headroom,
        })
        .ok_or(Unplaceable { refusals })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mllm_domain::resources::{Allocation, DeviceClaim, Sharing};

    const GIB: i64 = 1 << 30;

    fn footprint(bytes: i64, device: Option<(&str, Sharing)>) -> PhaseFootprint {
        PhaseFootprint {
            phase: ResourcePhase::Cold,
            allocations: vec![Allocation {
                domain: "unified".into(),
                bytes,
                host_kv_bytes: 0,
            }],
            devices: device
                .map(|(device, sharing)| {
                    vec![DeviceClaim {
                        device: device.into(),
                        sharing,
                    }]
                })
                .unwrap_or_default(),
        }
    }

    fn host(
        id: &str,
        managed: i64,
        instances_here: u32,
        charged: &[(&str, PhaseFootprint)],
    ) -> HostCandidate {
        HostCandidate {
            host_id: id.into(),
            eligible: true,
            instances_here,
            footprint: footprint(10 * GIB, None),
            limits: vec![MemoryLimit {
                domain: "unified".into(),
                managed_bytes: managed,
                free_reserve_bytes: 0,
                host_kv_bytes: None,
                parked_bytes: None,
            }],
            ledger: LedgerSnapshot {
                epoch: 1,
                owners: charged
                    .iter()
                    .map(|(owner, f)| ((*owner).into(), f.clone()))
                    .collect(),
            },
            max_parked: 4,
            occupied: false,
            whole_host: false,
        }
    }

    // ADR 0013 §4 step 3: spread prefers the host with fewest instances of
    // the deployment, then most headroom; ties break by host id.
    // T05 T27
    #[test]
    fn spread_prefers_fewest_instances_then_most_headroom_then_host_id() {
        let a = host("host-a", 100 * GIB, 1, &[]);
        let b = host("host-b", 100 * GIB, 0, &[]);
        let placed = place(&[a.clone(), b.clone()], "d", Strategy::Spread, None, None).unwrap();
        assert_eq!(placed.host_id, "host-b");
        let a = host(
            "host-a",
            100 * GIB,
            0,
            &[("other", footprint(40 * GIB, None))],
        );
        let placed = place(&[a, b.clone()], "d", Strategy::Spread, None, None).unwrap();
        assert_eq!(placed.host_id, "host-b", "more headroom wins");
        let a = host("host-a", 100 * GIB, 0, &[]);
        let placed = place(&[b, a], "d", Strategy::Spread, None, None).unwrap();
        assert_eq!(placed.host_id, "host-a", "a tie breaks by host id");
    }

    // ADR 0013 §4 step 3: pack prefers the host with most instances, then the
    // least headroom that still fits (best fit).
    // T05 T27
    #[test]
    fn pack_prefers_most_instances_then_best_fit() {
        let a = host("host-a", 100 * GIB, 0, &[]);
        let b = host("host-b", 100 * GIB, 1, &[]);
        let placed = place(&[a, b], "d", Strategy::Pack, None, None).unwrap();
        assert_eq!(placed.host_id, "host-b");
        let a = host(
            "host-a",
            100 * GIB,
            0,
            &[("other", footprint(80 * GIB, None))],
        );
        let b = host("host-b", 100 * GIB, 0, &[]);
        let placed = place(&[a, b], "d", Strategy::Pack, None, None).unwrap();
        assert_eq!(
            placed.host_id, "host-a",
            "best fit leaves the least headroom"
        );
        assert_eq!(placed.headroom_bytes, 10 * GIB);
    }

    // ADR 0013 §2, §4 step 1: max_per_host bounds each host; a host already at
    // the bound is refused with that reason.
    // T05 T14
    #[test]
    fn max_per_host_excludes_a_full_host() {
        let a = host("host-a", 100 * GIB, 1, &[]);
        let b = host("host-b", 100 * GIB, 1, &[]);
        let refused = place(&[a.clone(), b], "d", Strategy::Pack, Some(1), None).unwrap_err();
        assert_eq!(refused.code(), "max_per_host");
        let c = host("host-b", 100 * GIB, 0, &[]);
        assert_eq!(
            place(&[a, c], "d", Strategy::Pack, Some(1), None)
                .unwrap()
                .host_id,
            "host-b"
        );
    }

    // A host that cannot take another launch (a single-claim agent that runs
    // one, or another instance of this deployment on a per-launch host) is
    // refused before its reservations are even considered.
    // T05 T24
    #[test]
    fn an_occupied_host_is_refused() {
        let mut a = host("host-a", 100 * GIB, 0, &[]);
        a.occupied = true;
        let b = host("host-b", 100 * GIB, 1, &[]);
        assert_eq!(
            place(&[a.clone(), b], "d", Strategy::Spread, None, None)
                .unwrap()
                .host_id,
            "host-b"
        );
        assert_eq!(
            place(&[a], "d", Strategy::Spread, None, None)
                .unwrap_err()
                .code(),
            "host_occupied"
        );
    }

    // Owner decision 2026-09-23: a whole-host first start (its footprint is
    // the managed limit) is placed only on a host where no other owner holds
    // a charge, and is refused `startup_requires_empty_host` otherwise.
    // T26 T27
    #[test]
    fn a_whole_host_start_needs_a_host_without_other_charges() {
        let mut busy = host(
            "host-a",
            100 * GIB,
            0,
            &[("other", footprint(GIB, None))],
        );
        busy.footprint = footprint(100 * GIB, None);
        busy.whole_host = true;
        assert_eq!(
            place(
                std::slice::from_ref(&busy),
                "d",
                Strategy::Spread,
                None,
                None
            )
            .unwrap_err()
            .code(),
            "startup_requires_empty_host"
        );
        let mut empty = host("host-b", 100 * GIB, 0, &[]);
        empty.footprint = footprint(100 * GIB, None);
        empty.whole_host = true;
        assert_eq!(
            place(&[busy, empty], "d", Strategy::Spread, None, None)
                .unwrap()
                .host_id,
            "host-b"
        );
    }

    // ADR 0013 §4 step 1 (W12): an ineligible host is never a candidate.
    // T05 T33
    #[test]
    fn an_ineligible_host_is_never_chosen() {
        let mut a = host("host-a", 100 * GIB, 0, &[]);
        a.eligible = false;
        let b = host("host-b", 5 * GIB, 0, &[]);
        let refused = place(&[a.clone(), b], "d", Strategy::Spread, None, None).unwrap_err();
        assert_eq!(
            refused.refusals,
            vec![
                ("host-a".into(), HostRefusal::Ineligible),
                ("host-b".into(), HostRefusal::Insufficient)
            ]
        );
        assert_eq!(refused.code(), "no_host_fits");
        assert_eq!(
            place(&[a], "d", Strategy::Spread, None, None)
                .unwrap_err()
                .code(),
            "host_ineligible"
        );
    }

    // SPEC §7.3, ADR 0013 §4 step 2: on a one-GPU host two instances co-reside
    // only when both claims are shared; an exclusive claim conflicts.
    // T24 T26
    #[test]
    fn co_residence_on_one_device_needs_shared_claims() {
        let exclusive = footprint(10 * GIB, Some(("gpu0", Sharing::Exclusive)));
        let shared = footprint(10 * GIB, Some(("gpu0", Sharing::Shared)));
        let mut candidate = host(
            "host-a",
            100 * GIB,
            1,
            &[("deployment:d/instance:1", shared.clone())],
        );
        candidate.footprint = exclusive.clone();
        assert_eq!(
            place(
                std::slice::from_ref(&candidate),
                "d",
                Strategy::Pack,
                None,
                None
            )
            .unwrap_err()
            .code(),
            "device_conflict"
        );
        candidate.footprint = shared.clone();
        assert_eq!(
            place(
                std::slice::from_ref(&candidate),
                "d",
                Strategy::Pack,
                None,
                None
            )
            .unwrap()
            .host_id,
            "host-a"
        );
        let mut held = host(
            "host-a",
            100 * GIB,
            1,
            &[("deployment:d/instance:1", exclusive)],
        );
        held.footprint = shared;
        assert_eq!(
            place(&[held], "d", Strategy::Pack, None, None)
                .unwrap_err()
                .code(),
            "device_conflict"
        );
    }

    // ADR 0007, ADR 0013 §4 step 4 (T23): nothing is placed over budget; the
    // instance's own earlier reservation is not counted against it.
    // T23 T16
    #[test]
    fn nothing_is_placed_over_budget_and_the_owner_is_not_double_counted() {
        let full = host(
            "host-a",
            15 * GIB,
            0,
            &[("other", footprint(6 * GIB, None))],
        );
        assert_eq!(
            place(&[full], "d", Strategy::Spread, None, None)
                .unwrap_err()
                .code(),
            "insufficient_capacity"
        );
        let own = host(
            "host-a",
            15 * GIB,
            0,
            &[("d", footprint(10 * GIB, None))],
        );
        assert_eq!(
            place(&[own], "d", Strategy::Spread, None, None)
                .unwrap()
                .headroom_bytes,
            5 * GIB
        );
    }

    // ADR 0013 §4 step 5: a stopped instance keeps its last host as a
    // preference, and moves when that host is ineligible or full.
    // T05 T33
    #[test]
    fn a_stopped_instance_prefers_its_last_host_until_it_cannot_take_it() {
        let a = host("host-a", 100 * GIB, 1, &[]);
        let b = host("host-b", 100 * GIB, 0, &[]);
        assert_eq!(
            place(
                &[a.clone(), b.clone()],
                "d",
                Strategy::Spread,
                None,
                Some("host-a")
            )
            .unwrap()
            .host_id,
            "host-a"
        );
        let mut gone = a;
        gone.eligible = false;
        assert_eq!(
            place(&[gone, b], "d", Strategy::Spread, None, Some("host-a"))
                .unwrap()
                .host_id,
            "host-b"
        );
    }
}
