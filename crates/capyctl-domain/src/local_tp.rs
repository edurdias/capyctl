//! ADR 0035 §§5–7: the processes of one model on several GPUs of one host.
//!
//! A launch on N GPUs of one host (its device list, rank r on the r-th GPU)
//! is one single-host launch, never a group. Its rank processes are named at
//! readiness from a fresh per-process, per-GPU sample (§8): for each chosen
//! GPU, the owned process holding the most memory there is that GPU's rank
//! process. Kernel facts (pid, boot id, start ticks) identify them, never the
//! titles an engine sets. Nothing here observes anything; callers pass the
//! sample and the owned group they read.

use std::collections::BTreeMap;

use crate::completion::{ProcessIdentity, RANK_ROLE_PREFIX};
use crate::resources::ProcessResident;

/// ADR 0035 §6: no distinct rank process per GPU by the initialize timeout
/// (launch failure).
pub const TP_RANK_UNOBSERVED: &str = "tp_rank_unobserved";
/// ADR 0035 §7: a rank process exited (status only).
pub const ENGINE_RANK_EXITED: &str = "engine_rank_exited";
/// ADR 0035 §5: an owned process of a built-in engine listens off loopback
/// (launch failure).
pub const ENGINE_LISTENER_EXPOSED: &str = "engine_listener_exposed";
/// ADR 0035 §7: a request had no first token within the stall timeout and the
/// instance's own probe failed too (status only).
pub const TP_STALLED: &str = "tp_stalled";

/// ADR 0035 §6 and the evidence table: vLLM and SGLang run one process per
/// rank, so readiness names a rank process per GPU. llama.cpp runs one
/// process across every GPU of its layer split; it has no rank processes.
pub fn names_ranks(engine: &str) -> bool {
    matches!(engine, "vllm" | "sglang")
}

/// One rank process, as readiness named it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankRecord {
    /// The position of its GPU in the launch's device list.
    pub rank: u32,
    /// That GPU's host-local device id (`gpuN`).
    pub device: String,
    /// The process with the role it is recorded under.
    pub process: ProcessIdentity,
}

/// What one sample says about the ranks of a launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RankNaming {
    /// Each GPU has one distinct rank process. `identities` is the owned
    /// group with every rank process that is neither `api` nor a `worker-N`
    /// renamed `rank-N`; `ranks` is in rank order.
    Named {
        identities: Vec<ProcessIdentity>,
        ranks: Vec<RankRecord>,
    },
    /// Not (yet) evidence: why, in one operator-safe phrase.
    Unobserved(String),
}

/// ADR 0035 §6: name the rank processes of `identities` (the launch's owned
/// processes, as the group scan named them) from `sample` (a fresh
/// per-process, per-GPU reading) for the rank-ordered `devices`. A GPU no
/// owned process holds memory on, two owned processes holding the same most
/// there, or one process that would be the rank of two GPUs is no evidence.
/// A process sampled without per-GPU figures (an older reporter) is never a
/// rank. The LMCache server's group is never a rank.
pub fn name_ranks(
    identities: &[ProcessIdentity],
    sample: &[ProcessResident],
    devices: &[String],
) -> RankNaming {
    if devices.len() < 2 {
        return RankNaming::Unobserved("a launch on one GPU has no rank processes".into());
    }
    let mut ranks: Vec<RankRecord> = Vec::with_capacity(devices.len());
    for (rank, device) in devices.iter().enumerate() {
        let mut best: Option<(&ProcessIdentity, u64)> = None;
        let mut tied = false;
        for identity in identities.iter().filter(|identity| !identity.is_lmcache()) {
            let Some(bytes) = sample
                .iter()
                .filter(|resident| {
                    resident.pid == identity.pid
                        && resident.boot_id == identity.boot_id
                        && resident.start_ticks == identity.start_ticks
                })
                .filter_map(|resident| resident.bytes_on(device))
                .max()
                .filter(|bytes| *bytes > 0)
            else {
                continue;
            };
            match best {
                Some((_, most)) if bytes < most => {}
                Some((_, most)) if bytes == most => tied = true,
                _ => {
                    best = Some((identity, bytes));
                    tied = false;
                }
            }
        }
        let Some((process, _)) = best else {
            return RankNaming::Unobserved(format!(
                "no owned process holds memory on {device} (rank {rank})"
            ));
        };
        if tied {
            return RankNaming::Unobserved(format!(
                "two owned processes hold the same memory on {device} (rank {rank})"
            ));
        }
        if let Some(other) = ranks.iter().find(|r| r.process.same_process(process)) {
            return RankNaming::Unobserved(format!(
                "process {} would be the rank of {} and {device}",
                process.pid, other.device
            ));
        }
        let rank = u32::try_from(rank).unwrap_or(u32::MAX);
        let mut process = process.clone();
        if process.role != "api" && !is_worker_role(&process.role) {
            process.role = format!("{RANK_ROLE_PREFIX}{rank}");
        }
        ranks.push(RankRecord {
            rank,
            device: device.clone(),
            process,
        });
    }
    let identities = identities
        .iter()
        .map(|identity| {
            ranks
                .iter()
                .find(|r| r.process.same_process(identity))
                .map_or_else(|| identity.clone(), |r| r.process.clone())
        })
        .collect();
    RankNaming::Named { identities, ranks }
}

/// ADR 0027: a direct child of the engine's leader (`worker-N`).
fn is_worker_role(role: &str) -> bool {
    role.strip_prefix("worker-")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The readiness receipt's record of the ranks (ADR 0035 §6: a rank process
/// that is `api` or `worker-N` keeps its role and records its rank here):
/// `tp ranks: 0=worker-1:4243@gpu0 1=rank-1:4250@gpu1`.
pub fn ranks_receipt(ranks: &[RankRecord]) -> String {
    let entries: Vec<String> = ranks
        .iter()
        .map(|r| {
            format!(
                "{}={}:{}@{}",
                r.rank, r.process.role, r.process.pid, r.device
            )
        })
        .collect();
    format!("{RECEIPT_PREFIX}{}", entries.join(" "))
}

const RECEIPT_PREFIX: &str = "tp ranks: ";

/// One rank a receipt records: its rank, role, pid and device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedRank {
    pub rank: u32,
    pub role: String,
    pub pid: u32,
    pub device: String,
}

/// The ranks a readiness receipt records ([`ranks_receipt`]), in rank order;
/// empty when it records none or anything is malformed (naming only: a
/// receipt grants nothing).
pub fn recorded_ranks(receipt: &str) -> Vec<RecordedRank> {
    let Some(start) = receipt.find(RECEIPT_PREFIX) else {
        return Vec::new();
    };
    let tail = &receipt[start + RECEIPT_PREFIX.len()..];
    let tail = tail.split(';').next().unwrap_or_default();
    let mut ranks = BTreeMap::new();
    for entry in tail.split_whitespace() {
        let parsed = (|| {
            let (rank, rest) = entry.split_once('=')?;
            let (role, rest) = rest.split_once(':')?;
            let (pid, device) = rest.split_once('@')?;
            Some(RecordedRank {
                rank: rank.parse().ok()?,
                role: role.to_owned(),
                pid: pid.parse().ok()?,
                device: device.to_owned(),
            })
        })();
        let Some(parsed) = parsed.filter(|r| !r.role.is_empty() && !r.device.is_empty()) else {
            return Vec::new();
        };
        if ranks.insert(parsed.rank, parsed).is_some() {
            return Vec::new();
        }
    }
    ranks.into_values().collect()
}

/// ADR 0035 §7: the rank `process` was recorded as, by role and pid.
pub fn rank_of(receipt: &str, process: &ProcessIdentity) -> Option<RecordedRank> {
    recorded_ranks(receipt)
        .into_iter()
        .find(|r| r.role == process.role && r.pid == process.pid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::DeviceBytes;

    fn identity(role: &str, pid: u32) -> ProcessIdentity {
        ProcessIdentity {
            role: role.into(),
            pid,
            boot_id: "boot".into(),
            start_ticks: u64::from(pid) * 10,
        }
    }

    fn resident(pid: u32, per_device: &[(&str, u64)]) -> ProcessResident {
        let per_device: Vec<DeviceBytes> = per_device
            .iter()
            .map(|(device, bytes)| DeviceBytes {
                device: (*device).into(),
                bytes: *bytes,
            })
            .collect();
        let device_bytes = per_device.iter().map(|d| d.bytes as i64).sum();
        ProcessResident {
            pid,
            boot_id: "boot".into(),
            start_ticks: u64::from(pid) * 10,
            bytes: device_bytes,
            device_bytes,
            host_bytes: 0,
            per_device,
        }
    }

    fn devices() -> Vec<String> {
        vec!["gpu0".into(), "gpu1".into()]
    }

    // T46 (ADR 0035 §6): vLLM's workers are helpers of the engine core; the
    // process holding the most memory on each GPU is that GPU's rank and is
    // renamed `rank-N`; a context a rank opens on its peer's GPU loses to the
    // peer's own rank there.
    #[test]
    fn helpers_holding_each_gpu_become_rank_processes() {
        let group = vec![
            identity("api", 10),
            identity("worker-0", 11),
            identity("helper-0", 12),
            identity("helper-1", 13),
        ];
        let sample = vec![
            resident(11, &[("gpu0", 300 << 20)]),
            resident(12, &[("gpu0", 20 << 30), ("gpu1", 500 << 20)]),
            resident(13, &[("gpu1", 20 << 30), ("gpu0", 400 << 20)]),
        ];
        let RankNaming::Named { identities, ranks } = name_ranks(&group, &sample, &devices())
        else {
            panic!("named");
        };
        assert_eq!(
            identities
                .iter()
                .map(|i| i.role.as_str())
                .collect::<Vec<_>>(),
            ["api", "worker-0", "rank-0", "rank-1"]
        );
        assert_eq!(ranks[0].device, "gpu0");
        assert_eq!(ranks[1].process.pid, 13);
        assert!(identities.iter().all(ProcessIdentity::is_engine));
    }

    // T46 (ADR 0035 §6): SGLang's schedulers are `worker-N`; a rank process
    // that is `api` or a worker keeps its role and records its rank in the
    // receipt, which names it again for an exit.
    #[test]
    fn worker_ranks_keep_their_role_and_record_their_rank() {
        let group = vec![
            identity("api", 10),
            identity("worker-0", 11),
            identity("worker-1", 12),
            identity("worker-2", 13),
        ];
        let sample = vec![
            resident(11, &[("gpu0", 20 << 30)]),
            resident(13, &[("gpu1", 20 << 30)]),
        ];
        let RankNaming::Named { identities, ranks } = name_ranks(&group, &sample, &devices())
        else {
            panic!("named");
        };
        assert_eq!(identities, group, "no role changes");
        let receipt = format!("sglang ready; {}", ranks_receipt(&ranks));
        assert_eq!(
            receipt,
            "sglang ready; tp ranks: 0=worker-0:11@gpu0 1=worker-2:13@gpu1"
        );
        let rank = rank_of(&receipt, &identity("worker-2", 13)).unwrap();
        assert_eq!((rank.rank, rank.device.as_str()), (1, "gpu1"));
        assert!(rank_of(&receipt, &identity("worker-1", 12)).is_none());
        assert!(recorded_ranks("vllm ready; probe answered").is_empty());
        assert!(recorded_ranks("tp ranks: 0=worker-0:x@gpu0").is_empty());
        assert!(recorded_ranks("tp ranks: 0=a:1@gpu0 0=b:2@gpu1").is_empty());
    }

    // T46 (ADR 0035 §6): no evidence while a GPU has no owned process, while
    // two owned processes hold the same most memory on one GPU, or while one
    // process would be the rank of two GPUs. A process read without per-GPU
    // figures is never a rank, and the LMCache server's group never is.
    #[test]
    fn ambiguous_or_missing_ranks_are_no_evidence() {
        let group = vec![
            identity("api", 10),
            identity("helper-0", 12),
            identity("helper-1", 13),
            identity("lmcache", 20),
        ];
        let unobserved = |sample: &[ProcessResident]| {
            matches!(
                name_ranks(&group, sample, &devices()),
                RankNaming::Unobserved(_)
            )
        };
        assert!(unobserved(&[resident(12, &[("gpu0", 1 << 30)])]));
        assert!(unobserved(&[resident(
            12,
            &[("gpu0", 1 << 30), ("gpu1", 1 << 30)]
        )]));
        assert!(unobserved(&[
            resident(12, &[("gpu0", 1 << 30)]),
            resident(13, &[("gpu0", 1 << 30), ("gpu1", 2 << 30)]),
            resident(10, &[("gpu0", 1 << 30)]),
        ]));
        let mut older = resident(13, &[]);
        older.device_bytes = 20 << 30;
        assert!(unobserved(&[resident(12, &[("gpu0", 1 << 30)]), older]));
        assert!(unobserved(&[
            resident(12, &[("gpu0", 1 << 30)]),
            resident(20, &[("gpu1", 8 << 30)]),
        ]));
        // A process of another launch is never a rank.
        assert!(unobserved(&[
            resident(12, &[("gpu0", 1 << 30)]),
            resident(99, &[("gpu1", 8 << 30)]),
        ]));
        assert!(matches!(
            name_ranks(&group, &[], &devices()[..1]),
            RankNaming::Unobserved(_)
        ));
    }

    // T46 (ADR 0035 §6): a recorded rank is the same engine process whatever
    // role a later group scan gives it, and every proof requires it alive.
    #[test]
    fn every_proof_requires_each_recorded_rank() {
        use crate::completion::{engine_members, same_engine};
        let recorded = vec![
            identity("api", 10),
            identity("worker-0", 11),
            identity("rank-0", 12),
            identity("rank-1", 13),
        ];
        assert_eq!(engine_members(&recorded).len(), 4);
        let scanned = vec![
            identity("api", 10),
            identity("worker-0", 11),
            identity("helper-0", 12),
            identity("helper-1", 13),
        ];
        assert!(same_engine(&recorded, &scanned));
        assert!(!same_engine(&recorded, &scanned[..3]), "rank 1 gone");
        let mut replaced = scanned.clone();
        replaced[3].start_ticks += 1;
        assert!(!same_engine(&recorded, &replaced), "a replaced rank");
    }
}
