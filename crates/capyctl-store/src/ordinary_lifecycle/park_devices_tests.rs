//! ADR 0035 §9: a vLLM park or wake on several GPUs settles only when every
//! GPU's own reading proves it. CPU only; nothing here qualifies an engine.
use super::*;
use crate::ordinary_lifecycle::multi_gpu_tests::revisions;
use capyctl_config::effective::Residency;
use capyctl_domain::resources::{Allocation, DeviceBytes};

const GIB: i64 = 1 << 30;

fn identity(pid: u32) -> ProcessIdentity {
    ProcessIdentity {
        role: format!("rank-{pid}"),
        pid,
        boot_id: "boot".into(),
        start_ticks: u64::from(pid),
    }
}

/// A process of the sample holding `per_gpu` (GiB) on each GPU it is on.
fn resident(pid: u32, per_gpu: &[(&str, i64)]) -> ProcessResident {
    let per_device: Vec<DeviceBytes> = per_gpu
        .iter()
        .map(|(device, gib)| DeviceBytes {
            device: (*device).into(),
            bytes: (*gib * GIB) as u64,
        })
        .collect();
    let device_bytes = per_gpu.iter().map(|(_, gib)| gib * GIB).sum();
    ProcessResident {
        pid,
        boot_id: "boot".into(),
        start_ticks: u64::from(pid),
        bytes: device_bytes + GIB,
        device_bytes,
        host_bytes: GIB,
        per_device,
    }
}

fn read(domain: &str, at: i64) -> MemoryObservation {
    MemoryObservation {
        domain: domain.into(),
        capacity_bytes: 48 * GIB,
        available_bytes: 40 * GIB,
        sampled_at_ms: at,
    }
}

/// Two ranks (pids 10 and 11) on gpu0 and gpu1, each GPU's parked budget
/// 2 GiB.
fn evidence(kind: ResidencyKind) -> DeviceEvidence {
    DeviceEvidence {
        kind,
        gpus: ["gpu0", "gpu1"]
            .map(|gpu| GpuBudget {
                domain: gpu.into(),
                device: gpu.into(),
                parked_bytes: 2 * GIB,
            })
            .into(),
        identities: vec![identity(10), identity(11)],
    }
}

fn both_read(at: i64) -> Vec<MemoryObservation> {
    vec![read("gpu0", at), read("gpu1", at), read("system", at)]
}

// T46 T20 (ADR 0035 §9): a park settles only when every GPU's own reading,
// taken after the reply, shows the launch's processes at or below that GPU's
// parked budget. A GPU with no reading, a stale one, one over its budget, one
// showing none of the launch's processes, or a reading without per-GPU
// figures leaves it unproven; another process's bytes never count.
#[test]
fn a_park_on_two_gpus_settles_only_on_every_gpus_reading() {
    let park = evidence(ResidencyKind::Park);
    let parked = [resident(10, &[("gpu0", 2)]), resident(11, &[("gpu1", 1)])];
    assert_eq!(
        park.judge(&both_read(1_000), &parked, 1_000, 1_000),
        Ok(1_000)
    );
    // A foreign process holding most of gpu0 is not the launch's.
    let mut beside = parked.to_vec();
    beside.push(resident(99, &[("gpu0", 30)]));
    assert_eq!(
        park.judge(&both_read(1_000), &beside, 1_000, 1_000),
        Ok(1_000)
    );
    // Observed at its oldest GPU reading.
    let apart = [
        read("gpu0", 1_200),
        read("gpu1", 1_100),
        read("gpu1", 1_050),
    ];
    assert_eq!(park.judge(&apart, &parked, 1_000, 1_300), Ok(1_100));

    // gpu1 was not read.
    let no_gpu1 = [read("gpu0", 1_000), read("system", 1_000)];
    let why = park.judge(&no_gpu1, &parked, 1_000, 1_000).unwrap_err();
    assert!(why.contains("gpu1 has no fresh reading"), "{why}");
    // gpu1's reading predates the reply.
    let stale = [read("gpu0", 1_000), read("gpu1", 999)];
    let why = park.judge(&stale, &parked, 1_000, 1_000).unwrap_err();
    assert!(why.contains("gpu1 has no fresh reading"), "{why}");
    // ... or postdates now.
    let future = [read("gpu0", 1_000), read("gpu1", 1_001)];
    assert!(park.judge(&future, &parked, 1_000, 1_000).is_err());
    // gpu1 still holds 3 GiB of the launch: above its 2 GiB budget.
    let over = [resident(10, &[("gpu0", 1)]), resident(11, &[("gpu1", 3)])];
    let why = park
        .judge(&both_read(1_000), &over, 1_000, 1_000)
        .unwrap_err();
    assert!(why.contains("gpu1") && why.contains("above"), "{why}");
    // Bytes of one rank on the other GPU count toward that GPU.
    let spread = [
        resident(10, &[("gpu0", 1), ("gpu1", 1)]),
        resident(11, &[("gpu1", 2)]),
    ];
    let why = park
        .judge(&both_read(1_000), &spread, 1_000, 1_000)
        .unwrap_err();
    assert!(why.contains("gpu1") && why.contains("above"), "{why}");
    // The sample lists none of the launch's processes on gpu1.
    let unseen = [resident(10, &[("gpu0", 1)]), resident(99, &[("gpu1", 0)])];
    let why = park
        .judge(&both_read(1_000), &unseen, 1_000, 1_000)
        .unwrap_err();
    assert!(why.contains("gpu1 shows none"), "{why}");
    // An older host's reading has no per-GPU figures.
    let mut older = parked.to_vec();
    older[1].per_device.clear();
    let why = park
        .judge(&both_read(1_000), &older, 1_000, 1_000)
        .unwrap_err();
    assert!(why.contains("no per-GPU figures"), "{why}");
    // No reading of the launch at all.
    assert!(park.judge(&both_read(1_000), &[], 1_000, 1_000).is_err());
}

// T46 T21 (ADR 0035 §9): a wake settles only when every GPU's reading shows
// the launch above that GPU's parked budget; one GPU still at its parked
// residue, or unread, leaves it unproven.
#[test]
fn a_wake_on_two_gpus_settles_only_when_every_gpu_is_above_its_budget() {
    let wake = evidence(ResidencyKind::Restore);
    let woken = [resident(10, &[("gpu0", 20)]), resident(11, &[("gpu1", 20)])];
    assert_eq!(
        wake.judge(&both_read(5_000), &woken, 4_000, 5_000),
        Ok(5_000)
    );
    let half = [resident(10, &[("gpu0", 20)]), resident(11, &[("gpu1", 2)])];
    let why = wake
        .judge(&both_read(5_000), &half, 4_000, 5_000)
        .unwrap_err();
    assert!(why.contains("gpu1") && why.contains("not above"), "{why}");
    let unread = [read("gpu0", 5_000)];
    let why = wake.judge(&unread, &woken, 4_000, 5_000).unwrap_err();
    assert!(why.contains("gpu1 has no fresh reading"), "{why}");
}

// T46 T20 (ADR 0035 §9): only a vLLM launch on several GPUs that parks is
// judged per GPU, each GPU's budget its parked phase's allocation there,
// named by its host-local device id; a parked phase short of a GPU is
// corrupt, never judged on the GPUs it has.
#[test]
fn only_a_parking_vllm_launch_on_several_gpus_is_judged_per_gpu() {
    let (one, two) = revisions();
    assert!(!judged_per_gpu(&one), "one GPU settles on the reply");
    assert!(judged_per_gpu(&two));
    let mut restarts = two.clone();
    restarts.residency = Residency::RestartOnly;
    assert!(!judged_per_gpu(&restarts));
    let mut sglang = two.clone();
    sglang.profile.engine = Engine::Sglang;
    assert!(
        !judged_per_gpu(&sglang),
        "SGLang proves each GPU by its saver"
    );

    let allocation = |domain: &str, bytes| Allocation {
        domain: domain.into(),
        bytes,
        host_kv_bytes: 0,
    };
    let parked = PhaseFootprint {
        phase: ResourcePhase::Parked,
        allocations: vec![
            allocation("gpu0", 2 * GIB),
            allocation("gpu1", 3 * GIB),
            allocation("system", 8 * GIB),
        ],
        devices: Vec::new(),
    };
    assert_eq!(
        budgets(&two, &parked).unwrap(),
        vec![
            GpuBudget {
                domain: "gpu0".into(),
                device: "gpu0".into(),
                parked_bytes: 2 * GIB,
            },
            GpuBudget {
                domain: "gpu1".into(),
                device: "gpu1".into(),
                parked_bytes: 3 * GIB,
            },
        ]
    );
    let mut short = parked.clone();
    short.allocations.remove(1);
    assert!(matches!(
        budgets(&two, &short),
        Err(LifecycleError::CorruptStoredData)
    ));
}
