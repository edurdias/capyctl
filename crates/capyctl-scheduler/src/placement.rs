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
use std::collections::{BTreeMap, BTreeSet};

use capyctl_domain::resources::{
    claims_conflict, gib, validate_footprint, LedgerSnapshot, MemoryLimit, PhaseFootprint,
    ResourcePhase,
};

use crate::device_choice::{choose_device, DeviceOption};
use crate::switching::{choose_victims_within, Victim, VictimCandidate};

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
    /// Discrete GPU design §7: on a multi-GPU host, the instance resolved once
    /// per device it may run on. Empty on a unified host and whenever the
    /// deployment pins its device: `footprint` alone is judged, as before.
    pub device_options: Vec<DeviceOption>,
    /// ADR 0013 §4 step 5 for devices: the device the instance last ran on,
    /// when this host is the one it last ran on.
    pub preferred_device: Option<String>,
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
    /// Discrete GPU design §7: the device chosen on that host, when the host
    /// offered a choice (`HostCandidate::device_options`).
    pub device: Option<String>,
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
            candidate_fits(candidate, owner)
        };
        match verdict {
            Ok((headroom, device)) => fitting.push((candidate, headroom, device)),
            Err(refusal) => refusals.push((candidate.host_id.clone(), refusal)),
        }
    }
    refusals.sort_by(|a, b| a.0.cmp(&b.0));
    if let Some((candidate, headroom, device)) =
        preferred.and_then(|host| fitting.iter().find(|(c, _, _)| c.host_id == host))
    {
        return Ok(Placement {
            host_id: candidate.host_id.clone(),
            headroom_bytes: *headroom,
            device: device.clone(),
        });
    }
    fitting.sort_by(|(a, a_room, _), (b, b_room, _)| {
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
        .map(|(candidate, headroom, device)| Placement {
            host_id: candidate.host_id.clone(),
            headroom_bytes: *headroom,
            device: device.clone(),
        })
        .ok_or(Unplaceable { refusals })
}

/// Whether the instance fits on this host as the ledger stands, by
/// reservations alone: its one footprint, or, on a host offering a device
/// choice, the device `choose_device` picks (discrete GPU design §7). The
/// headroom and the chosen device, if any.
pub fn candidate_fits(
    candidate: &HostCandidate,
    owner: &str,
) -> Result<(i64, Option<String>), HostRefusal> {
    if candidate.device_options.is_empty() {
        return fits(
            &candidate.ledger,
            owner,
            &candidate.footprint,
            &candidate.limits,
            candidate.max_parked,
        )
        .map(|headroom| (headroom, None));
    }
    choose_device(
        &candidate.ledger,
        owner,
        &candidate.device_options,
        &candidate.limits,
        candidate.max_parked,
        candidate.preferred_device.as_deref(),
    )
    .map(|(device, headroom)| (headroom, Some(device)))
}

/// ADR 0028 §5: a group has fixed hosts, so placement chooses nothing; it
/// only validates that the member named for each host fits there, as that
/// host's own admission sees it. `owners` maps every named host to its
/// member's owner; `candidates` carry each host's member footprint. Every
/// refusing host is listed (a named host without a candidate is
/// ineligible), so the group is placed on all of its hosts or on none.
pub fn fits_group(
    candidates: &[HostCandidate],
    owners: &BTreeMap<String, String>,
) -> Result<(), Unplaceable> {
    let mut refusals = Vec::new();
    for (host, owner) in owners {
        let verdict = match candidates.iter().find(|c| &c.host_id == host) {
            None => Err(HostRefusal::Ineligible),
            Some(c) if !c.eligible => Err(HostRefusal::Ineligible),
            Some(c) if c.occupied => Err(HostRefusal::Occupied),
            Some(c) => candidate_fits(c, owner).map(drop),
        };
        if let Err(refusal) = verdict {
            refusals.push((host.clone(), refusal));
        }
    }
    if refusals.is_empty() {
        Ok(())
    } else {
        Err(Unplaceable { refusals })
    }
}

/// ADR 0028 §5: one named host of a waiting group as its admission sees it,
/// for planning the eviction that lets the group's member there fit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostCandidateView {
    /// The waiting member's owner on this host.
    pub owner: String,
    /// The ledger as this host's admission sees it, without the member.
    pub ledger: LedgerSnapshot,
    pub limits: Vec<MemoryLimit>,
    /// Judged on this host alone: a group parked here counts once, as its
    /// one member owner on this host (ADR 0028 §12).
    pub max_parked: usize,
    /// The READY instances on this host that may be released, in preference
    /// order ([`crate::switching::order_victims`]). A group victim is listed
    /// under its member owner on this host.
    pub victims: Vec<VictimCandidate>,
    /// Owners whose charge is not proven released or releasable (a group
    /// member `uncertain`): their charge stays counted and they are never
    /// released to make room (ADR 0028 §11).
    pub uncertain: BTreeSet<String>,
}

/// ADR 0028 §5, SPEC §11: the victims each named host of a waiting group
/// releases so that the group's member fits there, or `None` when any named
/// host cannot make room even by releasing every candidate it offers, in
/// which case nothing is evicted anywhere. `need` is each named host's
/// member footprint; a named host without a view cannot make room. Each
/// host's set is minimal and its park-or-stop choice is
/// [`crate::switching::choose_victims`]'s.
pub fn plan_group_eviction(
    per_host: &BTreeMap<String, HostCandidateView>,
    need: &BTreeMap<String, PhaseFootprint>,
) -> Option<BTreeMap<String, Vec<Victim>>> {
    plan_group_eviction_within(per_host, need, &|_, _| true)
}

/// [`plan_group_eviction`], keeping a park on a host only when `room` (the
/// host's fresh observation, see [`crate::switching::observed_room`]) also
/// accepts the state it leaves there.
pub fn plan_group_eviction_within(
    per_host: &BTreeMap<String, HostCandidateView>,
    need: &BTreeMap<String, PhaseFootprint>,
    room: &dyn Fn(&str, &LedgerSnapshot) -> bool,
) -> Option<BTreeMap<String, Vec<Victim>>> {
    // ADR 0028 §5, SPEC §11: validate all hosts before evicting. Every
    // host's set is computed first; one host that cannot make room refuses
    // the whole plan, so no host releases anything for it.
    let mut plan = BTreeMap::new();
    for (host, footprint) in need {
        let view = per_host.get(host)?;
        // Uncertainty keeps accounting: an unproven charge is never free room.
        let offered: Vec<VictimCandidate> = view
            .victims
            .iter()
            .filter(|v| !view.uncertain.contains(&v.owner))
            .cloned()
            .collect();
        let victims = choose_victims_within(
            &view.ledger,
            &view.owner,
            footprint,
            &view.limits,
            view.max_parked,
            &offered,
            &|state| room(host, state),
        )
        .ok()?;
        plan.insert(host.clone(), victims);
    }
    Some(plan)
}

/// Why `candidate` cannot take the instance, in the operator's terms: on the
/// domain short by the most, what the instance needs, what is free as the
/// ledger stands and the host's limit there. `None` when no managed limit is
/// short (the refusal was a host-KV, parked or count limit).
pub fn shortfall(candidate: &HostCandidate, owner: &str) -> Option<String> {
    let bytes = |f: &PhaseFootprint, domain: &str| -> i64 {
        f.allocations
            .iter()
            .filter(|a| a.domain == domain)
            .map(|a| a.bytes)
            .sum()
    };
    let footprints: Vec<&PhaseFootprint> = if candidate.device_options.is_empty() {
        vec![&candidate.footprint]
    } else {
        candidate
            .device_options
            .iter()
            .map(|o| &o.footprint)
            .collect()
    };
    // On a host offering a device choice, the device closest to fitting.
    footprints
        .into_iter()
        .flat_map(|footprint| {
            candidate.limits.iter().filter_map(move |limit| {
                let need = bytes(footprint, &limit.domain);
                let used: i64 = candidate
                    .ledger
                    .owners
                    .iter()
                    .filter(|(id, _)| id.as_str() != owner)
                    .map(|(_, f)| bytes(f, &limit.domain))
                    .sum();
                let free = (limit.managed_bytes - used).max(0);
                (need > free).then_some((need - free, need, free, limit))
            })
        })
        .min_by_key(|(short, ..)| *short)
        .map(|(_, need, free, limit)| {
            format!(
                "needs {} of {} memory, {} free of its {} limit",
                gib(need),
                limit.domain,
                gib(free),
                gib(limit.managed_bytes)
            )
        })
}

impl Unplaceable {
    /// Each refusing host and its reason, naming the limit for a host that
    /// is short of memory, e.g. `host a needs 64.0 GiB of gpu0 memory,
    /// 60.8 GiB free of its 60.8 GiB limit`.
    pub fn detail(&self, candidates: &[HostCandidate], owner: &str) -> String {
        if self.refusals.is_empty() {
            return "no allowed host resolved the deployment's revision".into();
        }
        self.refusals
            .iter()
            .map(|(host, refusal)| {
                let short = (*refusal == HostRefusal::Insufficient)
                    .then(|| candidates.iter().find(|c| &c.host_id == host))
                    .flatten()
                    .and_then(|c| shortfall(c, owner));
                match short {
                    Some(short) => format!("host {host} {short}"),
                    None => format!("host {host}: {}", refusal.code()),
                }
            })
            .collect::<Vec<_>>()
            .join("; ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use capyctl_domain::resources::{Allocation, DeviceClaim, Sharing};

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
                reserve_absorbs_unmanaged: false,
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
            device_options: vec![],
            preferred_device: None,
        }
    }

    // SPEC §14: a capacity refusal names the limit it hit: what the instance
    // needs, what is free and the host's limit there; other reasons keep
    // their closed code.
    // T05 T23
    #[test]
    fn a_refusal_names_the_memory_limit_it_hit() {
        let a = host("a", 15 * GIB, 0, &[("other", footprint(8 * GIB, None))]);
        let mut b = host("b", 100 * GIB, 0, &[]);
        b.eligible = false;
        let refused =
            place(&[a.clone(), b.clone()], "d", Strategy::Spread, None, None).unwrap_err();
        assert_eq!(
            refused.detail(&[a, b], "d"),
            "host a needs 10.0 GiB of unified memory, 7.0 GiB free of its 15.0 GiB limit; \
             host b: host_ineligible"
        );
        assert_eq!(
            Unplaceable { refusals: vec![] }.detail(&[], "d"),
            "no allowed host resolved the deployment's revision"
        );
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
        let mut busy = host("host-a", 100 * GIB, 0, &[("other", footprint(GIB, None))]);
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
        let own = host("host-a", 15 * GIB, 0, &[("d", footprint(10 * GIB, None))]);
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

    // Discrete GPU design §7: a host offering a device choice is placed on
    // the device with room, and a stopped instance keeps its last device
    // while it fits there.
    // T27
    #[test]
    fn a_multi_gpu_host_places_on_a_device() {
        use crate::device_choice::DeviceOption;
        let device = |id: &str, bytes: i64| DeviceOption {
            device: id.into(),
            domain: id.into(),
            footprint: PhaseFootprint {
                phase: ResourcePhase::Cold,
                allocations: vec![Allocation {
                    domain: id.into(),
                    bytes,
                    host_kv_bytes: 0,
                }],
                devices: vec![],
            },
        };
        let mut candidate = host("host-a", 100 * GIB, 0, &[]);
        candidate.limits = ["gpu0", "gpu1"]
            .iter()
            .zip([22 * GIB, 30 * GIB])
            .map(|(domain, managed)| MemoryLimit {
                domain: (*domain).into(),
                managed_bytes: managed,
                free_reserve_bytes: 0,
                reserve_absorbs_unmanaged: false,
                host_kv_bytes: None,
                parked_bytes: None,
            })
            .collect();
        candidate.device_options = vec![device("gpu0", 12 * GIB), device("gpu1", 12 * GIB)];
        let placed = place(
            std::slice::from_ref(&candidate),
            "d",
            Strategy::Spread,
            None,
            None,
        )
        .unwrap();
        assert_eq!(placed.device.as_deref(), Some("gpu1"));
        candidate.preferred_device = Some("gpu0".into());
        let placed = place(&[candidate.clone()], "d", Strategy::Spread, None, None).unwrap();
        assert_eq!(placed.device.as_deref(), Some("gpu0"));
        candidate.device_options = vec![device("gpu0", 40 * GIB), device("gpu1", 40 * GIB)];
        assert_eq!(
            place(&[candidate], "d", Strategy::Spread, None, None)
                .unwrap_err()
                .code(),
            "insufficient_capacity"
        );
        let unified = host("host-b", 100 * GIB, 0, &[]);
        assert_eq!(
            place(&[unified], "d", Strategy::Spread, None, None)
                .unwrap()
                .device,
            None
        );
    }
}

#[cfg(test)]
mod group_tests {
    use super::*;
    use crate::switching::{Release, Victim, VictimCandidate};
    use capyctl_domain::resources::Allocation;
    use std::collections::BTreeMap;

    const GIB: i64 = 1 << 30;

    fn unified(gib: i64, phase: ResourcePhase) -> PhaseFootprint {
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

    /// GiB a host has left beside the victims it lists.
    struct Free(i64);

    fn free(gib: i64) -> Free {
        Free(gib)
    }

    /// A READY instance charged `gib` GiB that parks to 1 GiB.
    fn victim(owner: &str, gib: i64) -> (VictimCandidate, PhaseFootprint) {
        (
            VictimCandidate {
                owner: owner.into(),
                serves_elsewhere: false,
                last_used_ms: 0,
                parked: Some(unified(1, ResourcePhase::Parked)),
            },
            unified(gib, ResourcePhase::Ready),
        )
    }

    /// One host's free GiB and the victims it lists.
    type Host<'a> = (&'a str, Free, &'a [(VictimCandidate, PhaseFootprint)]);

    /// Each host's limit is its free memory plus its victims' charges.
    fn views(hosts: &[Host<'_>]) -> BTreeMap<String, HostCandidateView> {
        hosts
            .iter()
            .map(|(host, Free(free), victims)| {
                let charged: i64 = victims
                    .iter()
                    .map(|(_, f)| f.allocations[0].bytes)
                    .sum::<i64>();
                let view = HostCandidateView {
                    owner: format!("member-on-{host}"),
                    ledger: LedgerSnapshot {
                        epoch: 1,
                        owners: victims
                            .iter()
                            .map(|(v, f)| (v.owner.clone(), f.clone()))
                            .collect(),
                    },
                    limits: vec![MemoryLimit {
                        domain: "unified".into(),
                        managed_bytes: free * GIB + charged,
                        free_reserve_bytes: 0,
                        reserve_absorbs_unmanaged: false,
                        host_kv_bytes: None,
                        parked_bytes: None,
                    }],
                    max_parked: 4,
                    victims: victims.iter().map(|(v, _)| v.clone()).collect(),
                    uncertain: BTreeSet::new(),
                };
                ((*host).to_owned(), view)
            })
            .collect()
    }

    /// Each named host's member footprint (cold), in GiB.
    fn need(hosts: &[(&str, i64)]) -> BTreeMap<String, PhaseFootprint> {
        hosts
            .iter()
            .map(|(host, gib)| ((*host).to_owned(), unified(*gib, ResourcePhase::Cold)))
            .collect()
    }

    fn names(victims: &[Victim]) -> Vec<&str> {
        victims.iter().map(|v| v.owner.as_str()).collect()
    }

    // T16, T27: if one named host cannot make room, nothing is evicted anywhere.
    #[test]
    fn eviction_is_all_or_nothing_across_hosts() {
        let views = views(&[
            ("host-a", free(10), &[victim("x", 80)]),
            ("host-b", free(10), &[]),
        ]);
        assert!(plan_group_eviction(&views, &need(&[("host-a", 80), ("host-b", 80)])).is_none());
    }

    // T16: each host evicts only what it needs.
    #[test]
    fn each_host_evicts_its_minimum() {
        let views = views(&[
            ("host-a", free(10), &[victim("x", 80), victim("y", 10)]),
            ("host-b", free(90), &[victim("z", 50)]),
        ]);
        let plan = plan_group_eviction(&views, &need(&[("host-a", 80), ("host-b", 80)])).unwrap();
        assert_eq!(names(&plan["host-a"]), ["x"]);
        assert!(plan["host-b"].is_empty());
    }

    // T16, T27: a named host without a view cannot make room, so nothing is
    // evicted on the hosts that could.
    #[test]
    fn a_named_host_without_a_view_evicts_nothing() {
        let views = views(&[("host-a", free(10), &[victim("x", 80)])]);
        assert!(plan_group_eviction(&views, &need(&[("host-a", 80), ("host-b", 1)])).is_none());
    }

    // T32, ADR 0028 §11: an uncertain member's charge is never free room,
    // even when it is offered as a victim.
    #[test]
    fn an_uncertain_member_is_never_released_for_room() {
        let mut views = views(&[
            ("host-a", free(10), &[victim("u", 80)]),
            ("host-b", free(90), &[]),
        ]);
        let need = need(&[("host-a", 80), ("host-b", 80)]);
        assert_eq!(
            names(&plan_group_eviction(&views, &need).unwrap()["host-a"]),
            ["u"]
        );
        views
            .get_mut("host-a")
            .unwrap()
            .uncertain
            .insert("u".into());
        assert!(plan_group_eviction(&views, &need).is_none());
    }

    // T27, ADR 0028 §12: `max_parked` counts a group once on each host. Each
    // host holds one parked member of group p; under `max_parked: 1` per host
    // the victim x stops on each host rather than parking beside it, and the
    // plan exists (a count across hosts would see two parked and refuse).
    #[test]
    fn max_parked_counts_a_group_once_per_host() {
        let mut views = views(&[
            ("host-a", free(10), &[victim("x", 80)]),
            ("host-b", free(10), &[victim("x2", 80)]),
        ]);
        for (host, view) in &mut views {
            view.max_parked = 1;
            view.ledger.owners.insert(
                format!("p-member-on-{host}"),
                unified(0, ResourcePhase::Parked),
            );
        }
        let plan = plan_group_eviction(&views, &need(&[("host-a", 80), ("host-b", 80)])).unwrap();
        assert_eq!(plan["host-a"][0].release, Release::Stop);
        assert_eq!(plan["host-b"][0].release, Release::Stop);
        for view in views.values_mut() {
            view.max_parked = 2;
        }
        let plan = plan_group_eviction(&views, &need(&[("host-a", 80), ("host-b", 80)])).unwrap();
        assert_eq!(plan["host-a"][0].release, Release::Park);
        assert_eq!(plan["host-b"][0].release, Release::Park);
    }

    // T16, ADR 0028 §5: a group has fixed hosts; placement only validates
    // that its member fits on every one of them, each judged as its own
    // admission sees it.
    #[test]
    fn a_group_fits_only_when_every_named_host_fits() {
        let host = |id: &str, free: i64| HostCandidate {
            host_id: id.into(),
            eligible: true,
            instances_here: 0,
            footprint: unified(10, ResourcePhase::Cold),
            limits: vec![MemoryLimit {
                domain: "unified".into(),
                managed_bytes: free * GIB,
                free_reserve_bytes: 0,
                reserve_absorbs_unmanaged: false,
                host_kv_bytes: None,
                parked_bytes: None,
            }],
            ledger: LedgerSnapshot {
                epoch: 1,
                owners: BTreeMap::new(),
            },
            max_parked: 4,
            occupied: false,
            whole_host: false,
            device_options: vec![],
            preferred_device: None,
        };
        let owners: BTreeMap<String, String> = [("host-a", "m0"), ("host-b", "m1")]
            .map(|(h, o)| (h.to_owned(), o.to_owned()))
            .into();
        assert!(fits_group(&[host("host-a", 20), host("host-b", 20)], &owners).is_ok());
        let refused = fits_group(&[host("host-a", 20), host("host-b", 5)], &owners).unwrap_err();
        assert_eq!(
            refused.refusals,
            [("host-b".to_owned(), HostRefusal::Insufficient)]
        );
        let mut occupied = host("host-a", 20);
        occupied.occupied = true;
        assert_eq!(
            fits_group(&[occupied, host("host-b", 20)], &owners)
                .unwrap_err()
                .code(),
            "host_occupied"
        );
        assert_eq!(
            fits_group(&[host("host-a", 20)], &owners)
                .unwrap_err()
                .refusals,
            [("host-b".to_owned(), HostRefusal::Ineligible)]
        );
    }
}
