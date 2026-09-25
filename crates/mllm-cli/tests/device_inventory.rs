//! The host's device-inventory publication path.
//!
//! SPEC §3: the NVIDIA inventory digest and the selected device's physical
//! UUID are host facts published at boot, and they are the two prerequisites
//! the SGLang placement gate needs (the host document the launch is qualified
//! against must carry them). The Python collector itself is stubbed here —
//! `collect_with` takes the seam the boot uses — so these tests are about the
//! closed parse and what the published table states, never about a real GPU.
//! Nothing here qualifies a native engine recipe (SPEC §18).

use std::path::Path;

use mllm_cli::device_inventory::{collect_with, is_physical_uuid};
use mllm_cli::standalone_config;
use mllm_config::engine_policy::Engine;
use mllm_controller::EngineInstallation;

const DIGEST: &str = "2124d5550ed2316a62493cd335399bea795ffa074e07207c8b1d2f3a729387dd";
const UUID: &str = "GPU-09631200-fdff-a345-295f-a1a6f84b2f84";

fn inventory_json(devices: serde_json::Value) -> String {
    serde_json::json!({
        "schema": "mllm-nvidia-inventory-v1",
        "host_id": "host-a",
        "digest": DIGEST,
        "devices": devices,
    })
    .to_string()
}

fn named(
    installation: &EngineInstallation,
) -> Vec<mllm_controller::engine_provider::NamedInstallation> {
    vec![mllm_controller::engine_provider::NamedInstallation {
        profile: "local".into(),
        installation: installation.clone(),
    }]
}

fn installation() -> EngineInstallation {
    EngineInstallation {
        engine: Engine::Sglang,
        executable: "/opt/venv/bin/python3".into(),
        build_fingerprint: "0.5.20".into(),
        engine_config: serde_json::json!({"memory": {"kv_cache": "16GiB"}}),
        deep_park: true,
        trust_remote_code: false,
        models_root: "/srv/models".into(),
        runtime_dir: "/opt/mllm/runtime".into(),
        args: Vec::new(),
        installation_drift: Default::default(),
        cuda_home: None,
        engine_ports: (8100, 8199),
    }
}

/// The boot publishes the digest, and — because the inventory holds exactly one
/// device, the shape this host's single `gpu0` policy can honestly name — the
/// device's physical UUID on the device entry.
#[test]
fn a_boot_with_an_inventory_publishes_the_digest_and_the_single_devices_uuid() {
    let published = collect_with(Path::new("/any"), &|_root| {
        Ok(inventory_json(serde_json::json!([
            {"physical_gpu_uuid": UUID, "pci_address": "000f:01:00.0",
             "device_minor": 0, "vendor_id": "0x10de", "device_id": "0x2e12"}
        ])))
    })
    .expect("a well-formed inventory publishes");
    assert_eq!(published.digest, DIGEST);
    assert_eq!(published.host_id, "host-a");
    assert_eq!(published.physical_gpu_uuid.as_deref(), Some(UUID));

    let host =
        standalone_config::host_policy(&named(&installation()), "env-1", 1 << 40, Some(&published));
    assert_eq!(host["device_inventory_digest"], DIGEST);
    assert_eq!(host["name"], "host-a");
    assert_eq!(
        host["resource_policy"]["devices"]["gpu0"]["physical_gpu_uuid"],
        UUID
    );
    // The published table must actually resolve: what a deployment is qualified
    // against is the table, not the JSON fragment a test read.
    mllm_config::effective::resolve_effective(
        &standalone_config::deployment_document(
            "m",
            "m",
            &mllm_config::effective::ModelSource::Local {
                path: "/srv/models/m".into(),
            },
            Engine::Sglang,
            1 << 40,
            standalone_config::DEFAULT_REQUEST_DEADLINE,
            true,
            "local",
        ),
        &host,
    )
    .expect("the published host resolves with its placement evidence");
}

/// A host with no NVIDIA inventory — the collector printed nothing, refused,
/// overran its bound, or produced a malformed document — publishes nothing:
/// no digest, no UUID. Honest absence, not an invented placeholder.
#[test]
fn a_boot_without_an_inventory_publishes_nothing() {
    let outcomes: Vec<std::io::Result<String>> = vec![
        Err(std::io::Error::other("the collector refused")),
        Ok(String::new()),
        Ok("device_observation_denied".to_string()),
        Ok(inventory_json(serde_json::json!([]))),
        Ok(format!(
            "{{\"digest\": \"{}\", \"devices\": []}}",
            "a".repeat(64)
        )),
        Ok(format!(
            "{{\"digest\": \"{}\", \"devices\": [{{\"physical_gpu_uuid\": \"{UUID}\"}}]}}",
            "Z".repeat(64)
        )),
        Ok(inventory_json(serde_json::json!([
            {"physical_gpu_uuid": "0", "pci_address": "000f:01:00.0",
             "device_minor": 0, "vendor_id": "0x10de", "device_id": "0x2e12"}
        ]))),
    ];
    for outcome in outcomes {
        let published = collect_with(Path::new("/any"), &|_root| match &outcome {
            Ok(text) => Ok(text.clone()),
            Err(error) => Err(std::io::Error::new(error.kind(), error.to_string())),
        });
        assert!(
            published.is_none(),
            "a host without an observable inventory publishes nothing: {outcome:?}"
        );
    }
    // And the published table carries neither field, so the host policy is
    // byte-identical to a host that never observed a device.
    let host = standalone_config::host_policy(&named(&installation()), "env-1", 1 << 40, None);
    assert!(
        host["device_inventory_digest"].is_null(),
        "no inventory, no digest"
    );
    assert!(
        host["resource_policy"]["devices"]["gpu0"]["physical_gpu_uuid"].is_null(),
        "no inventory, no device UUID"
    );
}

/// More than one device is a placement choice a boot does not make by itself:
/// the digest is published (the inventory is real), the UUID is not, and an
/// SGLang deployment then fails placement closed at the native gate.
#[test]
fn a_multi_device_inventory_publishes_the_digest_but_names_no_device() {
    let published = collect_with(Path::new("/any"), &|_root| {
        Ok(inventory_json(serde_json::json!([
            {"physical_gpu_uuid": UUID, "pci_address": "000f:01:00.0",
             "device_minor": 0, "vendor_id": "0x10de", "device_id": "0x2e12"},
            {"physical_gpu_uuid": "GPU-11111111-2222-3333-4444-555555555555",
             "pci_address": "000f:02:00.0", "device_minor": 1,
             "vendor_id": "0x10de", "device_id": "0x2e12"}
        ])))
    })
    .expect("a real inventory publishes its digest");
    assert_eq!(published.digest, DIGEST);
    assert_eq!(published.host_id, "host-a");
    assert_eq!(published.physical_gpu_uuid, None);
    let host =
        standalone_config::host_policy(&named(&installation()), "env-1", 1 << 40, Some(&published));
    assert_eq!(host["device_inventory_digest"], DIGEST);
    assert_eq!(host["name"], "host-a");
    assert!(host["resource_policy"]["devices"]["gpu0"]["physical_gpu_uuid"].is_null());
}

/// The UUID parser refuses exactly what `runtime/sglang_device.py` refuses:
/// the `GPU-` prefix and the 8-4-4-4 lowercase hex shape.
#[test]
fn the_uuid_shape_refuses_what_the_collector_refuses() {
    assert!(is_physical_uuid(UUID));
    assert!(is_physical_uuid("GPU-1a2b3c4d-5e6f-7a8b-9c0d-1e2f3a4b5c6d"));
    for bad in [
        "",
        "gpu-1a2b3c4d-5e6f-7a8b-9c0d-1e2f3a4b5c6d",
        "GPU-1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d",
        "GPU-1A2B3C4D-5E6F-7A8B-9C0D-1E2F3A4B5C6D",
        "GPU-1a2b3c4d_5e6f-7a8b-9c0d-1e2f3a4b5c6d",
        "GPU-1a2b3c4d-5e6f-7a8b-9c0d-1e2f3a4b5c6",
        "0",
    ] {
        assert!(!is_physical_uuid(bad), "{bad} is not a physical UUID");
    }
}
