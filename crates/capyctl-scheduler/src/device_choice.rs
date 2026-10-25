//! ADR 0019 (discrete GPU design §7, owner decision 3): on a multi-GPU host
//! capyctl picks the GPU.
//!
//! Each GPU is its own device-memory domain, and an instance that does not
//! pin a device is resolved once per device of the host. The choice is the
//! device where the instance fits with the most headroom on that device's own
//! domain, or, when none fits, the device whose release of READY instances is
//! smallest. Pure and deterministic, like the host choice it refines: nothing
//! here reserves or releases anything (ADR 0007).
//!
//! ADR 0035 §2: an instance on N GPUs of one host is resolved once per set of
//! N identical GPUs, and each option is a set: it fits when every GPU of it
//! holds its rank's charge, sets rank by their smallest headroom, then by
//! their GPUs' indices, and when none fits the set whose joined minimal
//! victims are fewest is evicted for. One GPU is a set of one, so every
//! single-GPU choice is exactly what it was.

use capyctl_domain::resources::{LedgerSnapshot, MemoryLimit, PhaseFootprint};

use crate::placement::{fits, HostRefusal};
use crate::switching::{Victim, VictimCandidate};

/// One device (or, ADR 0035 §2, one set of devices) of the host the instance
/// may run on, with the footprint it resolved to there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceOption {
    /// The key the choice is resolved and recorded under: the host's device
    /// id (`gpuN`), or for a set of N GPUs their ids in rank order joined by
    /// `,` (`gpu0,gpu1`, ADR 0035 §2).
    pub device: String,
    /// The device-memory domain each device maps to in the host policy
    /// (`devices.<id>.domain`), in rank order; one for a single GPU.
    /// Standalone names each after its device.
    pub domains: Vec<String>,
    /// The activation footprint as resolved with this device (set) selected.
    pub footprint: PhaseFootprint,
}

/// The device's driver index (`gpuN` → N); anything else sorts last.
fn index(device: &str) -> u32 {
    device
        .strip_prefix("gpu")
        .and_then(|n| n.parse().ok())
        .unwrap_or(u32::MAX)
}

/// ADR 0035 §2: the driver indices of a set's GPUs in rank order, which ties
/// between sets compare (lowest indices first). A single GPU's is its index.
fn indices(key: &str) -> Vec<u32> {
    key.split(',').map(index).collect()
}

/// What is left of `domain`'s managed limit once `footprint` is charged
/// beside every other owner. Only the device's own domain ranks devices: the
/// system domain is shared by all of them, so it would tie them.
fn device_headroom(
    ledger: &LedgerSnapshot,
    owner: &str,
    footprint: &PhaseFootprint,
    domain: &str,
    limits: &[MemoryLimit],
) -> i64 {
    let Some(limit) = limits.iter().find(|l| l.domain == domain) else {
        return i64::MIN;
    };
    let charged = |f: &PhaseFootprint| {
        f.allocations
            .iter()
            .filter(|a| a.domain == domain)
            .fold(0_i64, |sum, a| sum.saturating_add(a.bytes))
    };
    let used = ledger
        .owners
        .iter()
        .filter(|(id, _)| id.as_str() != owner)
        .fold(charged(footprint), |sum, (_, f)| {
            sum.saturating_add(charged(f))
        });
    limit.managed_bytes.saturating_sub(used)
}

/// ADR 0035 §2: a set's headroom is its smallest GPU's: the GPU closest to
/// full decides how much room the set leaves. One GPU's is its own.
fn set_headroom(
    ledger: &LedgerSnapshot,
    owner: &str,
    option: &DeviceOption,
    limits: &[MemoryLimit],
) -> i64 {
    option
        .domains
        .iter()
        .map(|domain| device_headroom(ledger, owner, &option.footprint, domain, limits))
        .min()
        .unwrap_or(i64::MIN)
}

/// Discrete GPU design §7: the device the instance fits on without eviction.
/// `preferred` (the device a stopped instance last ran on, ADR 0013 §4) wins
/// when it fits; otherwise the most headroom on the device's own domain, ties
/// by device index. Returns the device and the host's bottleneck headroom
/// with it there (what host ordering compares). `Err` carries the refusal of
/// the last device judged when none fits.
///
/// ADR 0035 §2: for sets, a set fits when every GPU holds its rank's charge
/// (the all-domain fit), the set whose smallest headroom is largest wins,
/// then the lowest indices, and a stopped instance's last set wins when it
/// fits.
pub fn choose_device(
    ledger: &LedgerSnapshot,
    owner: &str,
    options: &[DeviceOption],
    limits: &[MemoryLimit],
    max_parked: usize,
    preferred: Option<&str>,
) -> Result<(String, i64), HostRefusal> {
    let mut fitting = Vec::new();
    let mut last = HostRefusal::Insufficient;
    for option in options {
        match fits(ledger, owner, &option.footprint, limits, max_parked) {
            Ok(headroom) => fitting.push((
                option,
                headroom,
                set_headroom(ledger, owner, option, limits),
            )),
            Err(refusal) => last = refusal,
        }
    }
    if let Some((option, headroom, _)) =
        preferred.and_then(|p| fitting.iter().find(|(o, _, _)| o.device == p))
    {
        return Ok((option.device.clone(), *headroom));
    }
    fitting.sort_by(|(a, _, room_a), (b, _, room_b)| {
        room_b
            .cmp(room_a)
            .then(indices(&a.device).cmp(&indices(&b.device)))
            .then_with(|| a.device.cmp(&b.device))
    });
    fitting
        .first()
        .map(|(option, headroom, _)| (option.device.clone(), *headroom))
        .ok_or(last)
}

/// Discrete GPU design §7 (W10 per device): when no device fits, the device
/// whose minimal victim set is smallest, then whose victims were least
/// recently used (the sum of their `last_used_ms`), then the lower index.
/// Only owners charged on that device's own domain are its candidates.
/// `victims` arrive in preference order (`order_victims`); the order is kept,
/// and each chosen victim carries its release (park or stop, design §5).
pub fn choose_device_with_eviction(
    ledger: &LedgerSnapshot,
    owner: &str,
    options: &[DeviceOption],
    limits: &[MemoryLimit],
    max_parked: usize,
    victims: &[VictimCandidate],
) -> Result<(String, Vec<Victim>), HostRefusal> {
    choose_device_with_eviction_within(ledger, owner, options, limits, max_parked, victims, &|_| {
        true
    })
}

/// [`choose_device_with_eviction`], keeping a park only where `room` accepts
/// the state it leaves (see [`crate::switching::choose_victims_within`]).
///
/// ADR 0035 §2: for a set of GPUs the candidates are the owners charged on
/// any of its GPUs' domains, and the victims are the union of what each GPU
/// needs released, pruned to a minimal set for the whole footprint: an owner
/// charged on several of its GPUs (another multi-GPU instance) is one victim,
/// parked or stopped whole, counted once. The set with the fewest victims
/// wins, then the least recently used, then the lowest indices.
pub fn choose_device_with_eviction_within(
    ledger: &LedgerSnapshot,
    owner: &str,
    options: &[DeviceOption],
    limits: &[MemoryLimit],
    max_parked: usize,
    victims: &[VictimCandidate],
    room: &dyn Fn(&LedgerSnapshot) -> bool,
) -> Result<(String, Vec<Victim>), HostRefusal> {
    let mut best: Option<(&DeviceOption, Vec<Victim>, i64)> = None;
    let mut last = HostRefusal::Insufficient;
    for option in options {
        // Only owners charged on this GPU (one of this set's GPUs) can make
        // room on it.
        let here: Vec<VictimCandidate> = victims
            .iter()
            .filter(|v| {
                ledger.owners.get(&v.owner).is_some_and(|f| {
                    f.allocations
                        .iter()
                        .any(|a| option.domains.contains(&a.domain) && a.bytes > 0)
                })
            })
            .cloned()
            .collect();
        match crate::switching::choose_victims_within(
            ledger,
            owner,
            &option.footprint,
            limits,
            max_parked,
            &here,
            room,
        ) {
            Ok(chosen) => {
                let recency = here
                    .iter()
                    .filter(|v| chosen.iter().any(|c| c.owner == v.owner))
                    .fold(0_i64, |sum, v| sum.saturating_add(v.last_used_ms));
                let better = best.as_ref().is_none_or(|(o, c, r)| {
                    (chosen.len(), recency, indices(&option.device))
                        < (c.len(), *r, indices(&o.device))
                });
                if better {
                    best = Some((option, chosen, recency));
                }
            }
            Err(refusal) => last = refusal,
        }
    }
    best.map(|(option, chosen, _)| (option.device.clone(), chosen))
        .ok_or(last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use capyctl_domain::resources::{Allocation, ResourcePhase};

    const GIB: i64 = 1 << 30;

    fn footprint(allocations: &[(&str, i64)]) -> PhaseFootprint {
        PhaseFootprint {
            phase: ResourcePhase::Ready,
            allocations: allocations
                .iter()
                .map(|(domain, bytes)| Allocation {
                    domain: (*domain).into(),
                    bytes: *bytes,
                    host_kv_bytes: 0,
                })
                .collect(),
            devices: vec![],
        }
    }

    fn ledger_with<const N: usize>(owners: [(&str, PhaseFootprint); N]) -> LedgerSnapshot {
        LedgerSnapshot {
            epoch: 1,
            owners: owners
                .into_iter()
                .map(|(owner, f)| (owner.to_string(), f))
                .collect(),
        }
    }

    fn victim(owner: &str, last_used_ms: i64) -> VictimCandidate {
        VictimCandidate {
            owner: owner.into(),
            serves_elsewhere: false,
            last_used_ms,
            parked: None,
        }
    }

    fn owners(victims: Vec<Victim>) -> Vec<String> {
        victims.into_iter().map(|v| v.owner).collect()
    }

    fn limits2() -> Vec<MemoryLimit> {
        let l = |d: &str, m| MemoryLimit {
            domain: d.into(),
            managed_bytes: m,
            free_reserve_bytes: GIB,
            reserve_absorbs_unmanaged: false,
            host_kv_bytes: None,
            parked_bytes: Some(2 * GIB),
        };
        vec![
            l("gpu0", 22 * GIB),
            l("gpu1", 30 * GIB),
            l("system", 60 * GIB),
        ]
    }
    fn option(device: &str, bytes: i64) -> DeviceOption {
        DeviceOption {
            device: device.into(),
            domains: vec![device.into()],
            footprint: footprint(&[(device, bytes), ("system", 4 * GIB)]),
        }
    }
    fn options(bytes: i64) -> Vec<DeviceOption> {
        vec![option("gpu0", bytes), option("gpu1", bytes)]
    }

    // T27: the instance lands on the GPU with room.
    #[test]
    fn the_gpu_with_room_is_chosen() {
        let ledger = ledger_with([(
            "deployment:x/instance:0",
            footprint(&[("gpu1", 25 * GIB), ("system", 4 * GIB)]),
        )]);
        let (device, _) = choose_device(
            &ledger,
            "deployment:y/instance:0",
            &options(12 * GIB),
            &limits2(),
            4,
            None,
        )
        .unwrap();
        assert_eq!(device, "gpu0");
    }

    // T27: with both empty, the most headroom wins; a pin is honoured.
    #[test]
    fn headroom_then_pin() {
        let empty = ledger_with([]);
        assert_eq!(
            choose_device(&empty, "o", &options(12 * GIB), &limits2(), 4, None)
                .unwrap()
                .0,
            "gpu1"
        );
        assert_eq!(
            choose_device(
                &empty,
                "o",
                &[option("gpu0", 12 * GIB)],
                &limits2(),
                4,
                None
            )
            .unwrap()
            .0,
            "gpu0"
        );
        assert_eq!(
            choose_device(&empty, "o", &options(12 * GIB), &limits2(), 4, Some("gpu0"))
                .unwrap()
                .0,
            "gpu0"
        );
    }

    // T27: a device full on its own domain does not tie with an empty one
    // through the shared system domain; ties break by device index.
    #[test]
    fn devices_rank_by_their_own_domain_then_index() {
        let mut limits = limits2();
        limits.push(MemoryLimit {
            domain: "gpu2".into(),
            managed_bytes: 30 * GIB,
            free_reserve_bytes: GIB,
            reserve_absorbs_unmanaged: false,
            host_kv_bytes: None,
            parked_bytes: None,
        });
        let ledger = ledger_with([("a", footprint(&[("gpu1", 16 * GIB), ("system", 4 * GIB)]))]);
        let three = [
            option("gpu0", 4 * GIB),
            option("gpu1", 4 * GIB),
            option("gpu2", 4 * GIB),
        ];
        assert_eq!(
            choose_device(&ledger, "o", &three, &limits, 4, None)
                .unwrap()
                .0,
            "gpu2"
        );
        let equal = [option("gpu1", 4 * GIB), option("gpu0", 4 * GIB)];
        let mut same = limits2();
        same[1].managed_bytes = 22 * GIB;
        assert_eq!(
            choose_device(&ledger_with([]), "o", &equal, &same, 4, None)
                .unwrap()
                .0,
            "gpu0"
        );
    }

    // T27/T16: both full; the device needing fewer evictions is chosen, and only its owners are victims.
    #[test]
    fn eviction_is_per_device() {
        let ledger = ledger_with([
            ("a", footprint(&[("gpu0", 12 * GIB), ("system", 4 * GIB)])),
            // gpu1 is full: making 16 GiB of room there takes both b and c.
            ("b", footprint(&[("gpu1", 15 * GIB), ("system", 4 * GIB)])),
            ("c", footprint(&[("gpu1", 15 * GIB), ("system", 4 * GIB)])),
        ]);
        let victims = [victim("a", 3), victim("b", 1), victim("c", 2)];
        let (device, chosen) =
            choose_device_with_eviction(&ledger, "n", &options(16 * GIB), &limits2(), 4, &victims)
                .unwrap();
        assert_eq!(device, "gpu0");
        assert_eq!(owners(chosen), vec!["a".to_string()]);
    }

    // T27: equal victim counts; the least recently used set wins.
    #[test]
    fn equal_evictions_prefer_the_least_recently_used() {
        let ledger = ledger_with([
            ("a", footprint(&[("gpu0", 12 * GIB), ("system", 4 * GIB)])),
            ("b", footprint(&[("gpu1", 20 * GIB), ("system", 4 * GIB)])),
        ]);
        let victims = [victim("b", 1), victim("a", 9)];
        let (device, chosen) =
            choose_device_with_eviction(&ledger, "n", &options(16 * GIB), &limits2(), 4, &victims)
                .unwrap();
        assert_eq!(
            (device.as_str(), owners(chosen)),
            ("gpu1", vec!["b".to_string()])
        );
    }

    // No device can ever fit: the host's reason is returned.
    #[test]
    fn nothing_fits() {
        assert!(choose_device_with_eviction(
            &ledger_with([]),
            "n",
            &options(40 * GIB),
            &limits2(),
            4,
            &[]
        )
        .is_err());
    }

    // --- ADR 0035 §2: sets of GPUs -------------------------------------------

    /// Four identical GPUs of 24 GiB managed and host RAM.
    fn limits4() -> Vec<MemoryLimit> {
        let l = |d: &str, m| MemoryLimit {
            domain: d.into(),
            managed_bytes: m,
            free_reserve_bytes: GIB,
            reserve_absorbs_unmanaged: false,
            host_kv_bytes: None,
            parked_bytes: Some(2 * GIB),
        };
        vec![
            l("gpu0", 24 * GIB),
            l("gpu1", 24 * GIB),
            l("gpu2", 24 * GIB),
            l("gpu3", 24 * GIB),
            l("system", 100 * GIB),
        ]
    }

    /// The set of `gpus` (rank order), each GPU charged `per_gpu`.
    fn set(gpus: &[&str], per_gpu: i64) -> DeviceOption {
        let mut allocations: Vec<(&str, i64)> = gpus.iter().map(|g| (*g, per_gpu)).collect();
        allocations.push(("system", 7 * GIB));
        DeviceOption {
            device: gpus.join(","),
            domains: gpus.iter().map(|g| (*g).to_owned()).collect(),
            footprint: footprint(&allocations),
        }
    }

    /// Every pair of the four GPUs, lowest indices first.
    fn pairs(per_gpu: i64) -> Vec<DeviceOption> {
        let gpus = ["gpu0", "gpu1", "gpu2", "gpu3"];
        let mut out = Vec::new();
        for a in 0..gpus.len() {
            for b in a + 1..gpus.len() {
                out.push(set(&[gpus[a], gpus[b]], per_gpu));
            }
        }
        out
    }

    // T46: the set whose smallest headroom is largest wins, then the lowest
    // indices.
    #[test]
    fn sets_rank_by_their_smallest_headroom_then_indices() {
        let ledger = ledger_with([
            ("a", footprint(&[("gpu0", 8 * GIB), ("system", 4 * GIB)])),
            ("b", footprint(&[("gpu3", 2 * GIB), ("system", 4 * GIB)])),
        ]);
        let (device, _) =
            choose_device(&ledger, "n", &pairs(10 * GIB), &limits4(), 4, None).unwrap();
        assert_eq!(device, "gpu1,gpu2", "both GPUs empty beats one partly used");
        let (device, _) =
            choose_device(&ledger_with([]), "n", &pairs(10 * GIB), &limits4(), 4, None).unwrap();
        assert_eq!(device, "gpu0,gpu1", "equal headroom: the lowest indices");
        // A set fits only when every GPU of it holds its rank.
        let full = ledger_with([("a", footprint(&[("gpu0", 20 * GIB), ("system", 4 * GIB)]))]);
        let only = [set(&["gpu0", "gpu1"], 10 * GIB)];
        assert!(choose_device(&full, "n", &only, &limits4(), 4, None).is_err());
    }

    // T46: a stopped instance's last set wins when it fits, and only then.
    #[test]
    fn the_last_set_wins_when_it_fits() {
        let empty = ledger_with([]);
        let (device, _) = choose_device(
            &empty,
            "n",
            &pairs(10 * GIB),
            &limits4(),
            4,
            Some("gpu2,gpu3"),
        )
        .unwrap();
        assert_eq!(device, "gpu2,gpu3");
        let busy = ledger_with([("a", footprint(&[("gpu3", 20 * GIB), ("system", 4 * GIB)]))]);
        let (device, _) = choose_device(
            &busy,
            "n",
            &pairs(10 * GIB),
            &limits4(),
            4,
            Some("gpu2,gpu3"),
        )
        .unwrap();
        assert_eq!(device, "gpu0,gpu1");
    }

    // T46: when no set fits, the smallest union of victims wins; another
    // multi-GPU instance on both GPUs of a set is one victim, released whole.
    #[test]
    fn eviction_takes_the_smallest_union_and_a_multi_gpu_victim_whole() {
        let ledger = ledger_with([
            (
                "m",
                footprint(&[("gpu0", 12 * GIB), ("gpu1", 12 * GIB), ("system", 10 * GIB)]),
            ),
            ("s1", footprint(&[("gpu2", 12 * GIB), ("system", 4 * GIB)])),
            ("s2", footprint(&[("gpu3", 12 * GIB), ("system", 4 * GIB)])),
        ]);
        let victims = [victim("s1", 1), victim("s2", 2), victim("m", 3)];
        let (device, chosen) =
            choose_device_with_eviction(&ledger, "n", &pairs(16 * GIB), &limits4(), 4, &victims)
                .unwrap();
        assert_eq!(device, "gpu0,gpu1");
        assert_eq!(owners(chosen), vec!["m".to_string()]);
    }

    // T46: equal unions: the least recently used, then the lowest indices.
    #[test]
    fn equal_unions_prefer_the_least_recently_used_then_indices() {
        let ledger = ledger_with([
            (
                "m",
                footprint(&[("gpu0", 12 * GIB), ("gpu1", 12 * GIB), ("system", 10 * GIB)]),
            ),
            (
                "u",
                footprint(&[("gpu2", 12 * GIB), ("gpu3", 12 * GIB), ("system", 10 * GIB)]),
            ),
        ]);
        let (device, chosen) = choose_device_with_eviction(
            &ledger,
            "n",
            &pairs(16 * GIB),
            &limits4(),
            4,
            &[victim("u", 1), victim("m", 5)],
        )
        .unwrap();
        assert_eq!(
            (device.as_str(), owners(chosen)),
            ("gpu2,gpu3", vec!["u".into()])
        );
        let (device, _) = choose_device_with_eviction(
            &ledger,
            "n",
            &pairs(16 * GIB),
            &limits4(),
            4,
            &[victim("u", 5), victim("m", 5)],
        )
        .unwrap();
        assert_eq!(device, "gpu0,gpu1");
    }

    // T46 (ADR 0035 §3): a parked multi-GPU instance counts against each of
    // its GPUs' parked limit with that GPU's residue, and once in max_parked.
    #[test]
    fn a_parked_multi_gpu_instance_counts_per_gpu_and_once() {
        let mut parked = footprint(&[
            ("gpu0", 3 * GIB / 2),
            ("gpu1", 3 * GIB / 2),
            ("system", GIB),
        ]);
        parked.phase = capyctl_domain::resources::ResourcePhase::Parked;
        let ledger = ledger_with([("p", parked)]);
        let parks_on = |gpu: &str| {
            let mut f = footprint(&[(gpu, GIB), ("system", GIB)]);
            f.phase = capyctl_domain::resources::ResourcePhase::Parked;
            f
        };
        // gpu1 holds 1.5 GiB of p's residue: another 1 GiB passes its 2 GiB.
        assert_eq!(
            fits(&ledger, "n", &parks_on("gpu1"), &limits4(), 4),
            Err(HostRefusal::Insufficient)
        );
        // gpu2 holds none of it; p counts once, so two parked fit max 2.
        assert!(fits(&ledger, "n", &parks_on("gpu2"), &limits4(), 2).is_ok());
        assert_eq!(
            fits(&ledger, "n", &parks_on("gpu2"), &limits4(), 1),
            Err(HostRefusal::Insufficient)
        );
    }
}
