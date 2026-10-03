//! Discrete GPU design §4: the switch planner and the host's launch check
//! agree on every memory domain. Before the launch check read the device, the
//! planner saw room on a small card and the launch check read host RAM, so a
//! second model hung in CUDA allocation instead of evicting the first. One
//! fixture drives both sides here. CPU tests only: nothing here shows an
//! engine runs on a discrete GPU.

use capyctl_agent::{
    gpu_memory::{parse_query_gpu, GpuSample},
    memory::{parse_meminfo, HostMemorySample},
    native_execution::{admit_memory_with, LaunchVerdict},
};
use capyctl_config::effective::{DomainMemory, DomainPolicy, EffectiveDeployment};
use capyctl_domain::resources::{
    Allocation, LedgerSnapshot, MemoryLimit, PhaseFootprint, ResourcePhase,
};
use capyctl_scheduler::{
    placement::{fits, HostRefusal},
    switching::{choose_victims, Release, Victim, VictimCandidate},
};

const GIB: i64 = 1 << 30;
/// The device reserve of a 16 GiB card: 1.25 GiB.
const DEVICE_RESERVE: i64 = 1280 << 20;

/// `gpu0` managed 15 GiB, reserve 1.25 GiB, parked limit 2 GiB; `system`
/// managed 30 GiB, reserve 12 GiB, parked limit 15 GiB (61 GiB of RAM).
fn discrete_limits() -> Vec<MemoryLimit> {
    vec![
        MemoryLimit {
            domain: "gpu0".into(),
            managed_bytes: 15 * GIB,
            free_reserve_bytes: DEVICE_RESERVE,
            reserve_absorbs_unmanaged: false,
            host_kv_bytes: None,
            parked_bytes: Some(2 * GIB),
        },
        MemoryLimit {
            domain: "system".into(),
            managed_bytes: 30 * GIB,
            free_reserve_bytes: 12 * GIB,
            reserve_absorbs_unmanaged: false,
            host_kv_bytes: None,
            parked_bytes: Some(15 * GIB),
        },
    ]
}

fn footprint(phase: ResourcePhase, device: i64, system: i64) -> PhaseFootprint {
    let allocation = |domain: &str, bytes| Allocation {
        domain: domain.into(),
        bytes,
        host_kv_bytes: 0,
    };
    PhaseFootprint {
        phase,
        allocations: vec![allocation("gpu0", device), allocation("system", system)],
        devices: vec![],
    }
}

fn ready_footprint(device: i64, system: i64) -> PhaseFootprint {
    footprint(ResourcePhase::Ready, device, system)
}

fn cold_footprint(device: i64, system: i64) -> PhaseFootprint {
    footprint(ResourcePhase::Cold, device, system)
}

fn parked_footprint(device: i64, system: i64) -> PhaseFootprint {
    footprint(ResourcePhase::Parked, device, system)
}

fn ledger_with<const N: usize>(owners: [(impl Into<String>, PhaseFootprint); N]) -> LedgerSnapshot {
    LedgerSnapshot {
        epoch: 1,
        owners: owners
            .into_iter()
            .map(|(owner, footprint)| (owner.into(), footprint))
            .collect(),
    }
}

fn victim(owner: &str) -> VictimCandidate {
    VictimCandidate {
        owner: owner.into(),
        serves_elsewhere: false,
        last_used_ms: 1,
        // A deep park leaves the device residue and the host overhead.
        parked: Some(parked_footprint(GIB, 4 * GIB)),
    }
}

/// The same host as [`discrete_limits`], as the agent's resolved launch sees
/// it, with the launch's cold footprint on both domains.
fn discrete_effective(device: i64, system: i64) -> EffectiveDeployment {
    let source: serde_json::Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    let mut effective = capyctl_config::effective::resolve_effective(
        &source["input"]["deployment"],
        &source["input"]["host"],
    )
    .unwrap();
    let domain = |limit: &MemoryLimit, memory, device: Option<&str>| DomainPolicy {
        managed_limit: limit.managed_bytes,
        free_reserve: limit.free_reserve_bytes,
        host_kv_limit: limit.host_kv_bytes,
        parked_limit: limit.parked_bytes,
        memory,
        device: device.map(str::to_owned),
    };
    let limits = discrete_limits();
    effective.host.domains = [
        (
            "gpu0".to_string(),
            domain(&limits[0], DomainMemory::Device, Some("gpu0")),
        ),
        (
            "system".to_string(),
            domain(&limits[1], DomainMemory::Distinct, None),
        ),
    ]
    .into();
    effective.resources.cold.allocations = cold_footprint(device, system)
        .allocations
        .into_iter()
        .map(|a| capyctl_config::effective::Allocation {
            domain: a.domain,
            bytes: a.bytes,
            host_kv_bytes: a.host_kv_bytes,
        })
        .collect();
    effective
}

fn ram(capacity: i64, available: i64) -> HostMemorySample {
    parse_meminfo(
        &format!(
            "MemTotal: {} kB\nMemAvailable: {} kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n",
            capacity >> 10,
            available >> 10
        ),
        1,
    )
    .unwrap()
}

/// One 16 GiB card as `nvidia-smi` reports it, `used_mib` of it in use.
fn gpu(total_mib: i64, used_mib: i64) -> GpuSample {
    parse_query_gpu(
        &format!(
            "0, GPU-11111111-2222-3333-4444-555555555555, 00000000:01:00.0, RTX, {total_mib}, {used_mib}, {}\n",
            total_mib - used_mib
        ),
        1,
    )
    .unwrap()
}

// Two 8 GiB-weight models on a 16 GiB card, 61 GiB RAM. The planner evicts A
// for B; the launch check refuses B before A's release and admits it after.
// T16 T27
#[test]
fn planner_and_launch_check_agree_on_a_small_card() {
    let limits = discrete_limits();
    let a = ready_footprint(9 * GIB, 4 * GIB);
    let b_cold = cold_footprint(9 * GIB, 4 * GIB);
    let ledger = ledger_with([("deployment:a/instance:0", a)]);
    // Host RAM alone has room for both (8 GiB of 30); the device does not.
    let victims = choose_victims(
        &ledger,
        "deployment:b/instance:0",
        &b_cold,
        &limits,
        4,
        &[victim("deployment:a/instance:0")],
    )
    .unwrap();
    // Its parked residue fits beside B, so A parks rather than stops.
    assert_eq!(
        victims,
        vec![Victim {
            owner: "deployment:a/instance:0".to_string(),
            release: Release::Park
        }]
    );
    let effective_b = discrete_effective(9 * GIB, 4 * GIB);
    let before = gpu(16376, 9 * 1024 + 300); // A holds 9 GiB + context
    let after = gpu(16376, 1024); // A deep-parked: 1 GiB residue
    assert_eq!(
        admit_memory_with(&effective_b, &ram(61 * GIB, 50 * GIB), Some(&before)),
        Err(LaunchVerdict::Refused("insufficient_device_memory"))
    );
    assert!(admit_memory_with(&effective_b, &ram(61 * GIB, 50 * GIB), Some(&after)).is_ok());
    // A parked beside B fits the planner too: its residue is charged on the
    // device, its host overhead on the system domain.
    let parked = ledger_with([("deployment:a/instance:0", parked_footprint(GIB, 4 * GIB))]);
    assert!(fits(&parked, "deployment:b/instance:0", &b_cold, &limits, 4).is_ok());
}

// Review focus 5: three parked contexts plus a wake exceed the device parked limit.
// T16 T26
#[test]
fn parked_contexts_count_against_the_device() {
    let limits = discrete_limits();
    let parked = |n| {
        (
            format!("deployment:p{n}/instance:0"),
            parked_footprint(GIB, 4 * GIB),
        )
    };
    let ledger = ledger_with([parked(1), parked(2)]);
    let third = parked_footprint(GIB, 4 * GIB);
    assert_eq!(
        fits(&ledger, "deployment:p3/instance:0", &third, &limits, 4),
        Err(HostRefusal::Insufficient)
    );
}
