//! ADR 0019 (discrete GPU design §§2, 8): a host's memory domains as it
//! reports them, and the start-time check of a declared device domain against
//! the GPU that must back it.
//!
//! One builder serves every host role: the startup inventory a host publishes
//! when it connects, from one `/proc/meminfo` sample and one GPU sample, and
//! the device ids its later refreshes read. Standalone derives its policy from
//! the same observation, so a remote discrete host reports exactly what a
//! discrete standalone host observes.

use std::collections::BTreeMap;

use mllm_config::effective::{DomainMemory, HostPolicy};
use mllm_domain::resources::MemoryObservation;
use mllm_protocol::pb;

use crate::gpu_memory::{GpuSample, HostShape};

/// The device domains of an approved host document, each with the
/// nvidia-smi index its `gpuN` device names (`None`: a device id with no
/// index, which is never observed). A document whose policy does not resolve
/// declares none.
pub fn device_domains(document: &serde_json::Value) -> BTreeMap<String, DeviceDomain> {
    mllm_config::remote_resources::local_host_document(document)
        .ok()
        .and_then(|host| mllm_config::effective::normalize_host_policy(&host).ok())
        .map(|policy| {
            policy
                .domains
                .into_iter()
                .filter(|(_, d)| d.memory == DomainMemory::Device)
                .map(|(id, d)| {
                    let device = d.device.unwrap_or_default();
                    let index = crate::gpu_memory::device_index(&device);
                    (id, DeviceDomain { device, index })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// One declared device domain: its host-local device id and that device's
/// nvidia-smi index, when the id is `gpuN`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceDomain {
    pub device: String,
    pub index: Option<u32>,
}

/// The unknown observation (`-1`): it closes admission on its domain upstream.
fn unknown(domain: &str, kind: &str, sampled_at_ms: i64) -> pb::DomainObservation {
    pb::DomainObservation {
        domain_id: domain.into(),
        kind: kind.into(),
        observed_bytes: -1,
        observed_at_unix: sampled_at_ms / 1000,
        capacity_bytes: -1,
        available_bytes: -1,
        observed_at_unix_ms: sampled_at_ms,
        ..Default::default()
    }
}

/// SPEC §7.2 / ADR 0019: the domains a host publishes when it connects.
///
/// A `device` domain is read from its GPU in `gpu` (kind `device`, with its
/// device id); a device the sample does not report, or one without memory of
/// its own, is unknown. The host's RAM backs exactly one host domain: a
/// single `unified` or `distinct` system domain reads `memory`; several host
/// domains would need their own observers, so each is unknown rather than a
/// copied reading. A document without a resource policy reports one
/// `system` domain.
pub fn startup_domains(
    document: &serde_json::Value,
    memory: &MemoryObservation,
    gpu: Option<&GpuSample>,
) -> Vec<pb::DomainObservation> {
    let Some(declared) = document["resource_policy"]["domains"].as_object() else {
        return vec![pb::DomainObservation {
            domain_id: "system".into(),
            kind: "system".into(),
            observed_bytes: memory.available_bytes,
            observed_at_unix: memory.sampled_at_ms / 1000,
            capacity_bytes: memory.capacity_bytes,
            available_bytes: memory.available_bytes,
            observed_at_unix_ms: memory.sampled_at_ms,
            ..Default::default()
        }];
    };
    let devices = device_domains(document);
    let host_domains = declared
        .keys()
        .filter(|name| !devices.contains_key(*name))
        .count();
    declared
        .iter()
        .map(|(name, policy)| {
            if let Some(domain) = devices.get(name) {
                let observed = domain.index.and_then(|index| {
                    let sample = gpu?;
                    let memory = sample
                        .devices
                        .iter()
                        .find(|d| d.index == index)?
                        .memory
                        .as_ref()?;
                    Some((memory, sample.sampled_at_ms))
                });
                return match observed {
                    Some((device, sampled_at_ms)) => pb::DomainObservation {
                        domain_id: name.clone(),
                        kind: "device".into(),
                        device_id: domain.device.clone(),
                        observed_bytes: device.free_bytes,
                        observed_at_unix: sampled_at_ms / 1000,
                        capacity_bytes: device.total_bytes,
                        available_bytes: device.free_bytes,
                        observed_at_unix_ms: sampled_at_ms,
                        ..Default::default()
                    },
                    // SPEC §7.2: RAM never stands in for VRAM.
                    None => pb::DomainObservation {
                        device_id: domain.device.clone(),
                        ..unknown(name, "device", memory.sampled_at_ms)
                    },
                };
            }
            let supported = host_domains == 1
                && matches!(policy["memory"].as_str(), Some("unified" | "distinct"));
            if !supported {
                return unknown(name, "system", memory.sampled_at_ms);
            }
            pb::DomainObservation {
                domain_id: name.clone(),
                kind: "system".into(),
                observed_bytes: memory.available_bytes,
                observed_at_unix: memory.sampled_at_ms / 1000,
                capacity_bytes: memory.capacity_bytes,
                available_bytes: memory.available_bytes,
                observed_at_unix_ms: memory.sampled_at_ms,
                ..Default::default()
            }
        })
        .collect()
}

/// Discrete GPU design §2: a host's declared device domains checked against
/// the GPUs it observes, at start, before it connects. Every `device`
/// domain's `gpuN` device must be observed with memory of its own, its
/// declared `managed_limit + free_reserve` must fit the observed total, and a
/// UUID the policy states for it must be that GPU's. A device domain on a host
/// with no GPU or a unified (integrated) GPU is a mismatch. A policy without
/// device domains passes on any shape.
///
/// The error is `device_policy_mismatch: <domain> …`, naming the observed
/// total when there is one.
pub fn check_device_policy(policy: &HostPolicy, shape: &HostShape) -> Result<(), String> {
    for (name, domain) in &policy.domains {
        if domain.memory != DomainMemory::Device {
            continue;
        }
        let device = domain.device.as_deref().unwrap_or_default();
        let observed = match shape {
            HostShape::Discrete(devices) => crate::gpu_memory::device_index(device)
                .and_then(|index| devices.iter().find(|d| d.index == index)),
            HostShape::NoGpu | HostShape::Unified => None,
        };
        let Some(gpu) = observed else {
            let seen = match shape {
                HostShape::NoGpu => "this host has no NVIDIA GPU",
                HostShape::Unified => "this host's GPU shares host memory (unified)",
                HostShape::Discrete(_) => "no GPU with that index was observed",
            };
            return Err(format!(
                "device_policy_mismatch: {name} declares device {device}, but {seen}"
            ));
        };
        let Some(memory) = gpu.memory.as_ref() else {
            return Err(format!(
                "device_policy_mismatch: {name} declares device {device}, which has no memory of its own"
            ));
        };
        let declared = domain.managed_limit.saturating_add(domain.free_reserve);
        if declared > memory.total_bytes {
            return Err(format!(
                "device_policy_mismatch: {name} declares managed_limit + free_reserve = {declared} bytes, \
                 above the {} bytes observed on {device}",
                memory.total_bytes
            ));
        }
        let stated = policy
            .devices
            .get(device)
            .and_then(|d| d.physical_gpu_uuid.as_deref());
        if let Some(uuid) = stated.filter(|uuid| *uuid != gpu.uuid) {
            return Err(format!(
                "device_policy_mismatch: {name} states device {device} is {uuid}, but {} was observed \
                 (total {} bytes)",
                gpu.uuid, memory.total_bytes
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu_memory::{GpuDevice, GpuMemory};
    use serde_json::json;

    const MIB: i64 = 1 << 20;

    fn memory() -> MemoryObservation {
        MemoryObservation {
            domain: "system".into(),
            capacity_bytes: 64 << 30,
            available_bytes: 40 << 30,
            sampled_at_ms: 12_345,
        }
    }

    fn sample() -> GpuSample {
        GpuSample {
            devices: vec![GpuDevice {
                index: 0,
                uuid: "GPU-11111111-2222-3333-4444-555555555555".into(),
                pci_bus_id: "00000000:01:00.0".into(),
                name: "RTX".into(),
                memory: Some(GpuMemory {
                    total_bytes: 16376 * MIB,
                    used_bytes: 1536 * MIB,
                    free_bytes: 14840 * MIB,
                }),
            }],
            sampled_at_ms: 11_000,
        }
    }

    fn host(domains: serde_json::Value, devices: serde_json::Value) -> serde_json::Value {
        let golden: serde_json::Value = serde_json::from_str(include_str!(
            "../../mllm-config/tests/fixtures/effective-vllm-golden.json"
        ))
        .unwrap();
        let mut host = golden["input"]["host"].clone();
        host["resource_policy"]["domains"] = domains;
        host["resource_policy"]["devices"] = devices;
        host
    }

    fn discrete() -> serde_json::Value {
        host(
            json!({
                "system": {"memory": "distinct", "managed_limit": "24GiB", "free_reserve": "8GiB"},
                "gpu0": {"memory": "device", "device": "gpu0", "managed_limit": "14848MiB",
                         "free_reserve": "1528MiB", "parked_limit": "2GiB"}
            }),
            json!({"gpu0": {"domain": "gpu0", "sharing": "shared"}}),
        )
    }

    // T26 T29: the startup report reads RAM for the one system domain and the
    // GPU for the device domain (kind `device`, with its device id); without a
    // GPU sample the device is unknown, never RAM.
    #[test]
    fn a_discrete_host_reports_its_gpu_as_a_device_domain() {
        let report = startup_domains(&discrete(), &memory(), Some(&sample()));
        let find = |id: &str| report.iter().find(|d| d.domain_id == id).unwrap().clone();
        let gpu = find("gpu0");
        assert_eq!(
            (gpu.kind.as_str(), gpu.device_id.as_str()),
            ("device", "gpu0")
        );
        assert_eq!(gpu.capacity_bytes, 16376 * MIB);
        assert_eq!(gpu.available_bytes, 14840 * MIB);
        assert_eq!(gpu.observed_at_unix_ms, 11_000);
        let system = find("system");
        assert_eq!(system.kind, "system");
        assert!(system.device_id.is_empty());
        assert_eq!(
            (system.capacity_bytes, system.available_bytes),
            (64 << 30, 40 << 30)
        );

        let blind = startup_domains(&discrete(), &memory(), None);
        let gpu = blind.iter().find(|d| d.domain_id == "gpu0").unwrap();
        assert_eq!(
            (gpu.capacity_bytes, gpu.available_bytes, gpu.observed_bytes),
            (-1, -1, -1)
        );
        assert_eq!(gpu.kind, "device");
    }

    // T26: a unified host (today's two-host setup) reports exactly what it
    // did before device domains: one `system`-kind observation of RAM, no
    // device id; two host pools are each unknown rather than a copy.
    #[test]
    fn a_unified_host_reports_as_before() {
        let unified = host(
            json!({"unified": {"memory": "unified", "managed_limit": "96GiB", "free_reserve": "8GiB"}}),
            json!({"gpu0": {"domain": "unified", "sharing": "shared"}}),
        );
        let report = startup_domains(&unified, &memory(), Some(&sample()));
        assert_eq!(
            report,
            vec![pb::DomainObservation {
                domain_id: "unified".into(),
                kind: "system".into(),
                observed_bytes: 40 << 30,
                observed_at_unix: 12,
                capacity_bytes: 64 << 30,
                available_bytes: 40 << 30,
                observed_at_unix_ms: 12_345,
                ..Default::default()
            }]
        );
        let two = host(
            json!({
                "a": {"memory": "distinct", "managed_limit": "8GiB", "free_reserve": "1GiB"},
                "b": {"memory": "distinct", "managed_limit": "8GiB", "free_reserve": "1GiB"}
            }),
            json!({"gpu0": {"domain": "a", "sharing": "shared"}}),
        );
        assert!(startup_domains(&two, &memory(), None)
            .iter()
            .all(|d| d.available_bytes == -1 && d.capacity_bytes == -1));
        let mut bare = unified.clone();
        bare.as_object_mut().unwrap().remove("resource_policy");
        let report = startup_domains(&bare, &memory(), None);
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].domain_id, "system");
    }

    fn rtx(index: u32, total_mib: i64, used_mib: i64) -> GpuDevice {
        GpuDevice {
            index,
            uuid: format!("GPU-{index:08}-2222-3333-4444-555555555555"),
            pci_bus_id: format!("00000000:0{}:00.0", index + 1),
            name: "RTX".into(),
            memory: Some(GpuMemory {
                total_bytes: total_mib * MIB,
                used_bytes: used_mib * MIB,
                free_bytes: (total_mib - used_mib) * MIB,
            }),
        }
    }

    /// The policy standalone derives for one 16376 MiB card: reserve
    /// max(1 GiB, 8 %), managed the rest.
    fn discrete_policy_resolved() -> HostPolicy {
        let reserve = (16376 * MIB * 8 / 100).max(1 << 30);
        let mut document = discrete();
        document["resource_policy"]["domains"]["gpu0"]["managed_limit"] =
            json!(format!("{}B", 16376 * MIB - reserve));
        document["resource_policy"]["domains"]["gpu0"]["free_reserve"] =
            json!(format!("{reserve}B"));
        mllm_config::effective::normalize_host_policy(&document).unwrap()
    }

    // T03: a declared device domain that the GPU cannot back refuses the
    // start, naming the domain and the observed total; a device domain on a
    // host without a discrete GPU is a mismatch; a stated UUID must match.
    #[test]
    fn a_mismatched_device_policy_refuses_start() {
        let shape = HostShape::Discrete(vec![rtx(0, 16376, 0)]);
        assert_eq!(
            check_device_policy(&discrete_policy_resolved(), &shape),
            Ok(())
        );
        let mut policy = discrete_policy_resolved();
        policy.domains.get_mut("gpu0").unwrap().managed_limit = 20 << 30;
        let error = check_device_policy(&policy, &shape).unwrap_err();
        assert!(error.starts_with("device_policy_mismatch: gpu0"), "{error}");
        assert!(error.contains(&(16376 * MIB).to_string()), "{error}");
        for other in [HostShape::NoGpu, HostShape::Unified] {
            let error = check_device_policy(&discrete_policy_resolved(), &other).unwrap_err();
            assert!(error.starts_with("device_policy_mismatch: gpu0"), "{error}");
        }
        let elsewhere = HostShape::Discrete(vec![rtx(1, 16376, 0)]);
        assert!(check_device_policy(&discrete_policy_resolved(), &elsewhere).is_err());
        let mut uuid = discrete_policy_resolved();
        uuid.devices.get_mut("gpu0").unwrap().physical_gpu_uuid =
            Some("GPU-99999999-2222-3333-4444-555555555555".into());
        let error = check_device_policy(&uuid, &shape).unwrap_err();
        assert!(error.starts_with("device_policy_mismatch: gpu0"), "{error}");
        uuid.devices.get_mut("gpu0").unwrap().physical_gpu_uuid = Some(rtx(0, 1, 0).uuid);
        assert_eq!(check_device_policy(&uuid, &shape), Ok(()));
        // A unified or RAM-only policy has nothing to check on any shape.
        let unified = mllm_config::effective::normalize_host_policy(&host(
            json!({"unified": {"memory": "unified", "managed_limit": "96GiB", "free_reserve": "8GiB"}}),
            json!({"gpu0": {"domain": "unified", "sharing": "shared"}}),
        ))
        .unwrap();
        for shape in [HostShape::NoGpu, HostShape::Unified, shape] {
            assert_eq!(check_device_policy(&unified, &shape), Ok(()));
        }
    }
}
