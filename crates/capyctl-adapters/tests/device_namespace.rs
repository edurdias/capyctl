//! Discrete GPU design §7: on a host with several GPUs, every launch pins the
//! GPU placement chose, for vLLM and SGLang alike: by the physical UUID the
//! host published, or else by its driver index with
//! `CUDA_DEVICE_ORDER=PCI_BUS_ID`, so CUDA numbers the GPUs the way
//! `nvidia-smi` published them. A launch never inherits every GPU: with
//! several GPUs, one it can name neither way is refused. CPU tests only:
//! nothing here shows an engine runs on the GPU it was pinned to.

use capyctl_adapters::sglang::frozen_from_effective;
use capyctl_adapters::vllm::{plan_from_effective, render_command, VllmPlanError};
use capyctl_config::effective::{
    resolve_effective, CudaNamespace, DomainMemory, EffectiveDeployment,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

const UUID: &str = "GPU-11111111-1111-1111-1111-111111111111";

fn golden(engine: &str) -> (Value, Value) {
    let text = match engine {
        "vllm" => include_str!("../../capyctl-config/tests/fixtures/effective-vllm-golden.json"),
        _ => include_str!("../../capyctl-config/tests/fixtures/effective-sglang-golden.json"),
    };
    let source: Value = serde_json::from_str(text).unwrap();
    (
        source["input"]["deployment"].clone(),
        source["input"]["host"].clone(),
    )
}

/// The golden deployment on a host with `devices`, launched on `selected`.
fn on(engine: &str, devices: Value, selected: &str) -> EffectiveDeployment {
    let (mut deployment, mut host) = golden(engine);
    if engine == "vllm" {
        deployment["residency"] = json!("restart_only");
        host["runtime_profiles"]["local"]["security"]["deep_park"] = json!("disabled");
    }
    host["resource_policy"]["devices"] = devices;
    let claim = json!([{"id": selected, "sharing": "shared"}]);
    deployment["devices"] = claim.clone();
    for phase in ["cold", "ready", "parking", "wake"] {
        deployment["resources"][phase]["devices"] = claim.clone();
    }
    resolve_effective(&deployment, &host).unwrap()
}

fn two_gpus(uuid_on_gpu1: bool) -> Value {
    let mut devices = json!({
        "gpu0": {"domain": "unified", "sharing": "shared"},
        "gpu1": {"domain": "unified", "sharing": "shared"}
    });
    if uuid_on_gpu1 {
        devices["gpu1"]["physical_gpu_uuid"] = json!(UUID);
    }
    devices
}

fn vllm_env(effective: &EffectiveDeployment) -> BTreeMap<String, String> {
    let plan = plan_from_effective(effective, 8123, "l".into(), "/r".into()).unwrap();
    render_command(&plan)
        .unwrap()
        .env
        .into_iter()
        .filter(|(name, _)| name.starts_with("CUDA_"))
        .collect()
}

fn pinned(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

// T27 T21: the published UUID pins the GPU, on either engine.
#[test]
fn a_published_uuid_pins_the_gpu_on_both_engines() {
    let vllm = on("vllm", two_gpus(true), "gpu1");
    assert_eq!(
        vllm.cuda_namespace(),
        Ok(Some(CudaNamespace::Uuid(UUID.into())))
    );
    assert_eq!(vllm_env(&vllm), pinned(&[("CUDA_VISIBLE_DEVICES", UUID)]));

    let sglang = on("sglang", two_gpus(true), "gpu1");
    let frozen = frozen_from_effective(
        &sglang,
        "binding",
        "incarnation",
        "127.0.0.1:8123",
        "toy".into(),
        "inference".into(),
        "admin".into(),
    )
    .unwrap();
    let device = &frozen.metadata().device;
    assert_eq!(device.physical_gpu_uuid.as_deref(), Some(UUID));
    assert_eq!(device.cuda_pci_index, None);
}

// T27 T21: with no UUID published, the GPU is pinned by its driver index in
// PCI bus order, on either engine; it never inherits every GPU.
#[test]
fn an_unpublished_uuid_pins_the_gpu_by_its_pci_index() {
    let vllm = on("vllm", two_gpus(false), "gpu1");
    assert_eq!(vllm.cuda_namespace(), Ok(Some(CudaNamespace::PciIndex(1))));
    assert_eq!(
        vllm_env(&vllm),
        pinned(&[
            ("CUDA_DEVICE_ORDER", "PCI_BUS_ID"),
            ("CUDA_VISIBLE_DEVICES", "1")
        ])
    );
    // The other GPU of the same host has no UUID either: its own index.
    assert_eq!(
        vllm_env(&on("vllm", two_gpus(true), "gpu0")),
        pinned(&[
            ("CUDA_DEVICE_ORDER", "PCI_BUS_ID"),
            ("CUDA_VISIBLE_DEVICES", "0")
        ])
    );

    let sglang = on("sglang", two_gpus(false), "gpu1");
    let frozen = frozen_from_effective(
        &sglang,
        "binding",
        "incarnation",
        "127.0.0.1:8123",
        "toy".into(),
        "inference".into(),
        "admin".into(),
    )
    .unwrap();
    let device = &frozen.metadata().device;
    assert_eq!(device.physical_gpu_uuid, None);
    assert_eq!(device.cuda_pci_index, Some(1));
}

// T27 T21: with several GPUs, one named neither by a UUID nor by a `gpuN`
// index cannot be pinned, and the launch is refused on both engines rather
// than handed every GPU.
#[test]
fn a_gpu_that_cannot_be_pinned_is_refused_on_both_engines() {
    let unnamed = json!({
        "left": {"domain": "unified", "sharing": "shared"},
        "right": {"domain": "unified", "sharing": "shared"}
    });
    let vllm = on("vllm", unnamed.clone(), "right");
    assert!(vllm.cuda_namespace().is_err());
    assert!(matches!(
        plan_from_effective(&vllm, 8123, "l".into(), "/r".into()),
        Err(VllmPlanError::UnpinnableDevice)
    ));
    let sglang = on("sglang", unnamed, "right");
    assert!(frozen_from_effective(
        &sglang,
        "binding",
        "incarnation",
        "127.0.0.1:8123",
        "toy".into(),
        "inference".into(),
        "admin".into(),
    )
    .is_err());
}

// T27: a one-device unified host (a GB10) keeps the agent's own namespace, as
// before; a discrete GPU's own domain is always pinned, even alone.
#[test]
fn only_a_choice_or_a_discrete_gpu_pins() {
    let one = json!({"gpu0": {"domain": "unified", "sharing": "shared"}});
    let unified = on("vllm", one.clone(), "gpu0");
    assert_eq!(unified.cuda_namespace(), Ok(None));
    assert!(vllm_env(&unified).is_empty());

    let mut discrete = on("vllm", one, "gpu0");
    discrete.host.domains.get_mut("unified").unwrap().memory = DomainMemory::Device;
    discrete.host.domains.get_mut("unified").unwrap().device = Some("gpu0".into());
    assert_eq!(
        discrete.cuda_namespace(),
        Ok(Some(CudaNamespace::PciIndex(0)))
    );
}

// T21 T26, SPEC §7.2 (found live 2026-10-09 on a GB10): an SGLang launch on
// unified memory carries its Ready charge, which the compile-job count leaves
// out; on a discrete GPU's own domain it carries none.
#[test]
fn the_sglang_launch_carries_the_ready_charge_on_unified_memory_only() {
    let one = json!({"gpu0": {"domain": "unified", "sharing": "shared"}});
    let frozen = |effective: &EffectiveDeployment| {
        frozen_from_effective(
            effective,
            "binding",
            "incarnation",
            "127.0.0.1:8123",
            "toy".into(),
            "inference".into(),
            "admin".into(),
        )
        .unwrap()
    };
    let unified = on("sglang", one.clone(), "gpu0");
    // The golden deployment's Ready phase charges 8 GiB on `unified`.
    assert_eq!(frozen(&unified).unified_ready_bytes(), 8 << 30);

    let mut discrete = on("sglang", one, "gpu0");
    discrete.host.domains.get_mut("unified").unwrap().memory = DomainMemory::Device;
    discrete.host.domains.get_mut("unified").unwrap().device = Some("gpu0".into());
    assert_eq!(frozen(&discrete).unified_ready_bytes(), 0);
}
