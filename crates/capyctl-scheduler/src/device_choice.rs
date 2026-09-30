//! ADR 0019 (discrete GPU design §7, owner decision 3): on a multi-GPU host
//! capyctl picks the GPU.
//!
//! Each GPU is its own device-memory domain, and an instance that does not
//! pin a device is resolved once per device of the host. The choice is the
//! device where the instance fits with the most headroom on that device's own
//! domain, or, when none fits, the device whose release of READY instances is
//! smallest. Pure and deterministic, like the host choice it refines: nothing
//! here reserves or releases anything (ADR 0007).

use capyctl_domain::resources::{LedgerSnapshot, MemoryLimit, PhaseFootprint};

use crate::placement::{fits, HostRefusal};
use crate::switching::{Victim, VictimCandidate};

/// One device of the host the instance may run on, with the footprint it
/// resolved to there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceOption {
    /// The host's device id (`gpuN`).
    pub device: String,
    /// The device-memory domain the device maps to in the host policy
    /// (`devices.<id>.domain`). Standalone names it after the device.
    pub domain: String,
    /// The activation footprint as resolved with this device selected.
    pub footprint: PhaseFootprint,
}

/// The device's driver index (`gpuN` → N); anything else sorts last.
fn index(device: &str) -> u32 {
    device
        .strip_prefix("gpu")
        .and_then(|n| n.parse().ok())
        .unwrap_or(u32::MAX)
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

/// Discrete GPU design §7: the device the instance fits on without eviction.
/// `preferred` (the device a stopped instance last ran on, ADR 0013 §4) wins
/// when it fits; otherwise the most headroom on the device's own domain, ties
/// by device index. Returns the device and the host's bottleneck headroom
/// with it there (what host ordering compares). `Err` carries the refusal of
/// the last device judged when none fits.
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
                device_headroom(ledger, owner, &option.footprint, &option.domain, limits),
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
            .then(index(&a.device).cmp(&index(&b.device)))
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
        // Only owners charged on this GPU can make room on it.
        let here: Vec<VictimCandidate> = victims
            .iter()
            .filter(|v| {
                ledger.owners.get(&v.owner).is_some_and(|f| {
                    f.allocations
                        .iter()
                        .any(|a| a.domain == option.domain && a.bytes > 0)
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
                    (chosen.len(), recency, index(&option.device)) < (c.len(), *r, index(&o.device))
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
            domain: device.into(),
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
}
