//! SPEC §§3.1, 7.3 (per-launch claims, journal v4): host-side co-residence
//! admission.
//!
//! The controller places launches and remains the admission authority (SPEC
//! §3.1, §7). The host re-checks, from its own approved resource policy only,
//! that a new launch fits beside every launch its journal still claims: the
//! memory domain's managed and host-KV budgets, device sharing (an exclusive
//! claim conflicts with any other claim on the same device) and the leased
//! port. A retained claim the host cannot resolve any more is charged the
//! whole managed budget: uncertainty keeps its accounting, so nothing is
//! admitted beside it. A refusal is a closed, typed policy refusal reported
//! before anything is journaled or started.
use super::{refusal::LaunchVerdict, NativeHostExecution};
use crate::journal::{ClaimPhase, ClaimedLaunch};
use capyctl_config::effective::{
    DomainMemory, DomainPolicy, PhaseFootprint, RecipeFootprints, Sharing,
};
use capyctl_protocol::execution::{MemberAction, MemberCommand, SingleLaunchPlan};
use std::collections::BTreeMap;

/// What one launch is charged on one of this host's memory domains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Charge {
    pub bytes: i64,
    pub host_kv_bytes: i64,
    pub devices: Vec<(String, Sharing)>,
    pub port: Option<u16>,
}

fn amounts(footprint: &PhaseFootprint, domain: &str) -> (i64, i64) {
    footprint
        .allocations
        .iter()
        .filter(|a| a.domain == domain)
        .fold((0i64, 0i64), |(bytes, kv), a| {
            (
                bytes.saturating_add(a.bytes),
                kv.saturating_add(a.host_kv_bytes),
            )
        })
}

fn devices(footprints: &[&PhaseFootprint]) -> Vec<(String, Sharing)> {
    let mut all: Vec<(String, Sharing)> = Vec::new();
    for footprint in footprints {
        for claim in &footprint.devices {
            match all.iter_mut().find(|(id, _)| *id == claim.id) {
                // The strictest claim any phase makes on the device holds.
                Some((_, sharing)) if claim.sharing == Sharing::Exclusive => {
                    *sharing = Sharing::Exclusive
                }
                Some(_) => {}
                None => all.push((claim.id.clone(), claim.sharing)),
            }
        }
    }
    all
}

/// The charge of a launch in `phase`, from its resolved recipe footprints.
/// A launch that is parking, restoring or quarantined may hold any of its
/// footprints, so it is charged the largest of each.
pub(super) fn charge(
    resources: &RecipeFootprints,
    phase: ClaimPhase,
    domain: &str,
    port: Option<u16>,
) -> Charge {
    let phases: Vec<&PhaseFootprint> = match phase {
        ClaimPhase::Starting => vec![&resources.cold],
        ClaimPhase::Ready => vec![&resources.ready],
        ClaimPhase::Parked => vec![&resources.parked],
        ClaimPhase::Changing => vec![
            &resources.cold,
            &resources.ready,
            &resources.parking,
            &resources.parked,
            &resources.wake,
        ],
    };
    let (bytes, host_kv_bytes) = phases
        .iter()
        .map(|footprint| amounts(footprint, domain))
        .fold((0, 0), |(b, k), (bytes, kv)| (b.max(bytes), k.max(kv)));
    Charge {
        bytes,
        host_kv_bytes,
        devices: devices(&phases),
        port,
    }
}

/// SPEC §§3.1, 7.3, 9.1: what a wake is charged: the larger of its wake-phase
/// peak and the ready footprint it settles at, so a wake that fits only once
/// settled is not admitted beside launches its peak would crowd out.
pub(super) fn wake_charge(resources: &RecipeFootprints, domain: &str) -> Charge {
    let phases = [&resources.wake, &resources.ready];
    let (bytes, host_kv_bytes) = phases
        .iter()
        .map(|footprint| amounts(footprint, domain))
        .fold((0, 0), |(b, k), (bytes, kv)| (b.max(bytes), k.max(kv)));
    Charge {
        bytes,
        host_kv_bytes,
        devices: devices(&phases),
        port: None,
    }
}

/// Whether `new` fits beside `others` within the domain's budgets. The closed
/// refusal names the first rule broken.
pub(super) fn fits(
    new: &Charge,
    others: &[Charge],
    limit: &DomainPolicy,
) -> Result<(), &'static str> {
    if new.port.is_some() && others.iter().any(|other| other.port == new.port) {
        return Err("port_conflict");
    }
    // SPEC §7.3: two claims on one device conflict unless both are shared.
    let conflict = others.iter().any(|other| {
        other.devices.iter().any(|(id, sharing)| {
            new.devices.iter().any(|(new_id, new_sharing)| {
                id == new_id
                    && (*sharing == Sharing::Exclusive || *new_sharing == Sharing::Exclusive)
            })
        })
    });
    if conflict {
        return Err("device_conflict");
    }
    let total = |pick: fn(&Charge) -> i64| {
        others
            .iter()
            .map(pick)
            .try_fold(pick(new), |sum, value| sum.checked_add(value))
    };
    let bytes = total(|c| c.bytes);
    let host_kv = total(|c| c.host_kv_bytes);
    if bytes.is_none_or(|bytes| bytes > limit.managed_limit)
        || limit
            .host_kv_limit
            .is_some_and(|max| host_kv.is_none_or(|kv| kv > max))
    {
        return Err(shortage(limit));
    }
    Ok(())
}

/// The closed refusal for a domain that cannot hold a charge: a GPU's own
/// memory is named apart from host memory (discrete GPU design §4).
fn shortage(limit: &DomainPolicy) -> &'static str {
    match limit.memory {
        DomainMemory::Device => "insufficient_device_memory",
        DomainMemory::Unified | DomainMemory::Distinct => "insufficient_memory",
    }
}

/// SPEC §§3.1, 7.3, discrete GPU design §4: [`fits`] on every memory domain
/// the host declares, each with its own budget. `new` is the launch's charge
/// on a domain and `others` every claim's; a domain a footprint does not
/// touch is charged nothing there. The switch planner judges the same
/// domains, so the two agree on a discrete host.
pub(super) fn fits_every_domain(
    domains: &BTreeMap<String, DomainPolicy>,
    new: impl Fn(&str) -> Charge,
    others: impl Fn(&str, &DomainPolicy) -> Vec<Charge>,
) -> Result<(), &'static str> {
    if domains.is_empty() {
        return Err("unauthorized");
    }
    for (domain, limit) in domains {
        fits(&new(domain), &others(domain, limit), limit)?;
    }
    Ok(())
}

impl NativeHostExecution {
    /// SPEC §§3.1, 7.3: admit `command` beside every retained claim, from this
    /// host's own approved policy. The launch alone was already admitted by
    /// `admit_launch`.
    pub(super) fn admit_beside_claims(
        &self,
        command: &MemberCommand,
        claimed: &[ClaimedLaunch],
    ) -> Result<(), LaunchVerdict> {
        let refused = LaunchVerdict::Refused;
        if claimed.is_empty() {
            return Ok(());
        }
        let effective = self.resolve(command).map_err(|_| refused("unauthorized"))?;
        fits_every_domain(
            &effective.host.domains,
            |domain| {
                charge(
                    &effective.resources,
                    ClaimPhase::Starting,
                    domain,
                    super::leased_port(command),
                )
            },
            |domain, limit| self.claim_charges(claimed, domain, limit),
        )
        .map_err(refused)
    }

    /// SPEC §§3.1, 7.3, 9.1: admit waking `owner` (a parked launch this host
    /// claims) beside every other claim, charged its wake peak or its ready
    /// footprint, whichever is larger ([`wake_charge`]). The
    /// owner's leased port is its own, so only memory and devices are judged.
    /// A refusal leaves the launch parked.
    pub(super) fn admit_wake_beside_claims(
        &self,
        owner: &MemberCommand,
        claimed: &[ClaimedLaunch],
    ) -> Result<(), LaunchVerdict> {
        let refused = LaunchVerdict::Refused;
        if claimed.is_empty() {
            return Ok(());
        }
        let effective = self
            .resolve_retained(owner)
            .map_err(|_| refused("unauthorized"))?;
        fits_every_domain(
            &effective.host.domains,
            |domain| wake_charge(&effective.resources, domain),
            |domain, limit| self.claim_charges(claimed, domain, limit),
        )
        .map_err(refused)
    }

    /// What every retained claim is charged on `domain`, by its durable phase.
    fn claim_charges(
        &self,
        claimed: &[ClaimedLaunch],
        domain: &str,
        limit: &DomainPolicy,
    ) -> Vec<Charge> {
        claimed
            .iter()
            .map(|claim| {
                // ADR 0028 §5: a group member is charged on its own host like
                // a single launch, with the port it leases (none on a worker
                // that serves nothing).
                let port = super::leased_port(&claim.command);
                match &claim.command.action {
                    MemberAction::LaunchSingle(_) | MemberAction::Launch { .. } => self
                        .resolve_retained(&claim.command)
                        .ok()
                        .map(|retained| charge(&retained.resources, claim.phase, domain, port)),
                    _ => None,
                }
                // Unresolvable: it may hold the whole budget.
                .unwrap_or(Charge {
                    bytes: limit.managed_limit,
                    host_kv_bytes: limit.host_kv_limit.unwrap_or(0),
                    devices: Vec::new(),
                    port,
                })
            })
            .collect()
    }

    /// Admission of a reserved launch as provisioning and the refusal path
    /// see it: alone, then beside every claim this host retains.
    pub(super) fn admit_launch_here(
        &self,
        command: &MemberCommand,
        plan: &SingleLaunchPlan,
    ) -> Result<(), LaunchVerdict> {
        // Outside the journal's locks: the GPU is sampled now.
        self.admit_launch(command, plan, super::GpuReading::Now)?;
        let claimed = self
            .journal
            .claimed_launches(&command.identity.command_id)
            .map_err(|_| LaunchVerdict::Uncertain)?;
        self.admit_beside_claims(command, &claimed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use capyctl_config::effective::{Allocation, DeviceClaim};

    const MIB: i64 = 1 << 20;

    fn footprint(bytes: i64, sharing: Sharing) -> PhaseFootprint {
        PhaseFootprint {
            allocations: vec![Allocation {
                domain: "unified".into(),
                bytes,
                host_kv_bytes: 0,
            }],
            devices: vec![DeviceClaim {
                id: "gpu0".into(),
                sharing,
            }],
        }
    }

    fn recipe(sharing: Sharing) -> RecipeFootprints {
        RecipeFootprints {
            cold: footprint(256 * MIB, sharing),
            ready: footprint(128 * MIB, sharing),
            parking: footprint(128 * MIB, sharing),
            parked: footprint(32 * MIB, sharing),
            wake: footprint(256 * MIB, sharing),
        }
    }

    fn limit(managed: i64) -> DomainPolicy {
        DomainPolicy {
            managed_limit: managed,
            free_reserve: 0,
            host_kv_limit: None,
            parked_limit: None,
            memory: DomainMemory::Unified,
            device: None,
        }
    }

    // SPEC §7.3, D10: a starting launch is charged its cold footprint, a
    // ready one its ready footprint, a parked one its parked footprint and a
    // changing one the largest of each.
    // T24 T26
    #[test]
    fn a_claim_is_charged_by_the_phase_its_journal_proves() {
        let shared = recipe(Sharing::Shared);
        let bytes = |phase| charge(&shared, phase, "unified", None).bytes;
        assert_eq!(bytes(ClaimPhase::Starting), 256 * MIB);
        assert_eq!(bytes(ClaimPhase::Ready), 128 * MIB);
        assert_eq!(bytes(ClaimPhase::Parked), 32 * MIB);
        assert_eq!(bytes(ClaimPhase::Changing), 256 * MIB);
        assert_eq!(charge(&shared, ClaimPhase::Ready, "other", None).bytes, 0);
    }

    // SPEC §§3.1, 7.3, 9.1, D10: a wake passes through the wake phase (weights
    // restaged, KV re-mapped) before it settles at ready, so it is charged the
    // larger of the two; a wake whose peak does not fit stays parked.
    // T24 T26
    #[test]
    fn a_wake_is_charged_its_peak_not_only_its_ready_footprint() {
        let shared = recipe(Sharing::Shared);
        let woken = wake_charge(&shared, "unified");
        assert_eq!(woken.bytes, 256 * MIB);
        let other = [charge(&shared, ClaimPhase::Ready, "unified", Some(30000))];
        // Ready alone would fit (128 + 128); the wake peak does not.
        assert_eq!(
            fits(&woken, &other, &limit(256 * MIB)),
            Err("insufficient_memory")
        );
        assert_eq!(fits(&woken, &other, &limit(384 * MIB)), Ok(()));
        let mut settled = recipe(Sharing::Shared);
        settled.wake = footprint(64 * MIB, Sharing::Shared);
        assert_eq!(wake_charge(&settled, "unified").bytes, 128 * MIB);
    }

    // SPEC §§3.1, 7.3, D10: co-residence fits within the managed budget; one
    // byte over is refused `insufficient_memory`, never admitted.
    // T24 T26 T23
    #[test]
    fn co_residence_is_bounded_by_the_managed_budget() {
        let shared = recipe(Sharing::Shared);
        let new = charge(&shared, ClaimPhase::Starting, "unified", Some(30001));
        let others = [
            charge(&shared, ClaimPhase::Ready, "unified", Some(30000)),
            charge(&shared, ClaimPhase::Ready, "unified", Some(30002)),
        ];
        assert_eq!(fits(&new, &others, &limit(512 * MIB)), Ok(()));
        assert_eq!(
            fits(&new, &others, &limit(512 * MIB - 1)),
            Err("insufficient_memory")
        );
        let starting = charge(&shared, ClaimPhase::Starting, "unified", Some(30000));
        assert_eq!(
            fits(&new, &[starting], &limit(512 * MIB - 1)),
            Err("insufficient_memory")
        );
        let mut kv = limit(1 << 40);
        kv.host_kv_limit = Some(0);
        let mut heavy = new.clone();
        heavy.host_kv_bytes = 1;
        assert_eq!(fits(&heavy, &others, &kv), Err("insufficient_memory"));
    }

    // SPEC §§3.1, 7.3, 9.1: a wake is charged the woken launch's ready
    // footprint beside every other claim (its own leased port is not judged);
    // one byte over is refused `insufficient_memory`, and it stays parked.
    // T24 T26 T23
    #[test]
    fn a_wake_is_charged_its_ready_footprint_beside_the_others() {
        let shared = recipe(Sharing::Shared);
        let woken = charge(&shared, ClaimPhase::Ready, "unified", None);
        let others = [
            charge(&shared, ClaimPhase::Ready, "unified", Some(30000)),
            charge(&shared, ClaimPhase::Parked, "unified", Some(30001)),
        ];
        assert_eq!(fits(&woken, &others, &limit(288 * MIB)), Ok(()));
        assert_eq!(
            fits(&woken, &others, &limit(288 * MIB - 1)),
            Err("insufficient_memory")
        );
    }

    // SPEC §7.3: an exclusive claim on the device conflicts with any other
    // claim there; two shared claims co-reside. A leased port is never shared.
    // T24 T26 T34
    #[test]
    fn exclusive_devices_and_reused_ports_are_refused() {
        let shared = charge(
            &recipe(Sharing::Shared),
            ClaimPhase::Ready,
            "unified",
            Some(1),
        );
        let exclusive = charge(
            &recipe(Sharing::Exclusive),
            ClaimPhase::Starting,
            "unified",
            Some(2),
        );
        let roomy = limit(1 << 40);
        assert_eq!(
            fits(&exclusive, std::slice::from_ref(&shared), &roomy),
            Err("device_conflict")
        );
        assert_eq!(
            fits(&shared, std::slice::from_ref(&exclusive), &roomy),
            Err("device_conflict")
        );
        let mut next = shared.clone();
        next.port = Some(3);
        assert_eq!(fits(&next, std::slice::from_ref(&shared), &roomy), Ok(()));
        next.port = Some(1);
        assert_eq!(fits(&next, &[shared], &roomy), Err("port_conflict"));
    }

    // Discrete GPU design §4: on a discrete host each launch is charged on the
    // GPU and on host RAM, and co-residence is judged on every domain. A
    // parked launch's residue leaves room on the card; a ready one does not,
    // and the refusal names the device. Before, any host with more than one
    // domain refused every launch beside a claim `unauthorized`.
    // T24 T26 T16
    #[test]
    fn co_residence_is_judged_on_every_domain() {
        const GIB: i64 = 1 << 30;
        let two = |device: i64, system: i64| PhaseFootprint {
            allocations: vec![
                Allocation {
                    domain: "gpu0".into(),
                    bytes: device,
                    host_kv_bytes: 0,
                },
                Allocation {
                    domain: "system".into(),
                    bytes: system,
                    host_kv_bytes: 0,
                },
            ],
            devices: vec![DeviceClaim {
                id: "gpu0".into(),
                sharing: Sharing::Shared,
            }],
        };
        let model = RecipeFootprints {
            cold: two(9 * GIB, 4 * GIB),
            ready: two(9 * GIB, 4 * GIB),
            parking: two(9 * GIB, 4 * GIB),
            parked: two(GIB, 4 * GIB),
            wake: two(9 * GIB, 4 * GIB),
        };
        let mut device = limit(15 * GIB);
        device.memory = DomainMemory::Device;
        device.device = Some("gpu0".into());
        let mut system = limit(30 * GIB);
        system.memory = DomainMemory::Distinct;
        let domains: std::collections::BTreeMap<String, DomainPolicy> =
            [("gpu0".to_string(), device), ("system".to_string(), system)].into();
        let model = &model;
        let new = |domain: &str| charge(model, ClaimPhase::Starting, domain, Some(30001));
        let beside = |phase| {
            move |domain: &str, _: &DomainPolicy| vec![charge(model, phase, domain, Some(30000))]
        };
        assert_eq!(
            fits_every_domain(&domains, new, beside(ClaimPhase::Parked)),
            Ok(())
        );
        assert_eq!(
            fits_every_domain(&domains, new, beside(ClaimPhase::Ready)),
            Err("insufficient_device_memory")
        );
        // Host RAM binds too: the system domain is its own budget.
        let mut tight = domains.clone();
        tight.get_mut("system").unwrap().managed_limit = 8 * GIB - 1;
        assert_eq!(
            fits_every_domain(&tight, new, beside(ClaimPhase::Parked)),
            Err("insufficient_memory")
        );
        assert_eq!(
            fits_every_domain(&Default::default(), new, beside(ClaimPhase::Parked)),
            Err("unauthorized")
        );
    }
}
