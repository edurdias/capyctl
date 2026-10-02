//! The host's device-inventory publication path.
//!
//! SPEC §3: the NVIDIA inventory digest and the selected device's physical
//! UUID are host facts published at boot, and they are the two prerequisites
//! the SGLang placement gate needs (the host document the launch is qualified
//! against must carry them). Most tests stub the Python collector; the runtime
//! trust tests execute a small fixture instead. These test closed publication
//! and safe collection, never a real GPU.
//! Nothing here qualifies a native engine recipe (SPEC §18).

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use capyctl_agent::gpu_memory::{GpuDevice, GpuMemory, GpuSample, HostShape};
use capyctl_cli::device_inventory::{collect_with, is_physical_uuid};
use capyctl_cli::standalone_config;
use capyctl_config::engine_policy::Engine;
use capyctl_controller::EngineInstallation;

const DIGEST: &str = "2124d5550ed2316a62493cd335399bea795ffa074e07207c8b1d2f3a729387dd";
const UUID: &str = "GPU-09631200-fdff-a345-295f-a1a6f84b2f84";

fn collector_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let root = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let runtime = root.path().join("runtime");
    std::fs::create_dir(&runtime).unwrap();
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = inventory_json(serde_json::json!([{"physical_gpu_uuid": UUID}]));
    std::fs::write(
        runtime.join("sglang_device.py"),
        format!(
            "from pathlib import Path\nPath(__file__).with_name('executed').touch()\nprint({output:?})\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(
        runtime.join("sglang_device.py"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    (root, runtime)
}

// T21 T37: refuse unsafe collector code before it can run at role boot.
#[test]
fn unsafe_inventory_runtime_never_executes() {
    use std::os::unix::fs::symlink;
    for unsafe_part in ["module", "helper", "runtime", "ancestor", "symlink"] {
        let (root, runtime) = collector_fixture();
        let module = runtime.join("sglang_device.py");
        let helper = runtime.join("helper.py");
        let path = match unsafe_part {
            "module" => &module,
            "helper" => {
                std::fs::write(&helper, "# helper\n").unwrap();
                &helper
            }
            "runtime" => &runtime,
            "ancestor" => root.path(),
            "symlink" => {
                let original = root.path().join("collector.py");
                std::fs::rename(&module, &original).unwrap();
                symlink(original, &module).unwrap();
                &module
            }
            _ => unreachable!(),
        };
        if unsafe_part != "symlink" {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o777)).unwrap();
        }
        assert!(
            capyctl_cli::device_inventory::collect(&runtime, None).is_none(),
            "accepted unsafe {unsafe_part}"
        );
        assert!(
            !runtime.join("executed").exists(),
            "ran unsafe {unsafe_part}"
        );
        assert!(!root.path().join("executed").exists(), "ran symlink target");
    }
}

// T21 T37: use exactly the verified directory with isolated Python imports.
#[test]
fn trusted_inventory_runtime_uses_isolated_imports_and_relative_helpers() {
    let (root, runtime) = collector_fixture();
    let selected = root.path().join("custom-runtime");
    std::fs::rename(&runtime, &selected).unwrap();
    std::fs::write(
        selected.join("helper.py"),
        format!(
            "output = {:?}\n",
            inventory_json(serde_json::json!([{"physical_gpu_uuid": UUID}]))
        ),
    )
    .unwrap();
    std::fs::set_permissions(
        selected.join("helper.py"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    std::fs::write(
        selected.join("sglang_device.py"),
        "import sys\nassert sys.flags.isolated and sys.flags.no_site and sys.dont_write_bytecode\nfrom .helper import output\nprint(output)\n",
    )
    .unwrap();
    let poison = "from pathlib import Path\nPath(__file__).with_name('wrong-import').touch()\nraise RuntimeError('wrong import')\n";
    std::fs::write(root.path().join("runtime.py"), poison).unwrap();
    std::fs::write(root.path().join("sitecustomize.py"), poison).unwrap();
    capyctl_adapters::sglang::SglangLaunch::validate_wrapper_path(
        &selected.join("sglang_device.py"),
    )
    .expect("fixture path is trusted");
    capyctl_agent::runtime_integrity::verify(&selected, &["sglang_device.py"])
        .expect("fixture runtime is trusted");
    let published = capyctl_cli::device_inventory::collect(&selected, None)
        .expect("a trusted custom runtime must collect successfully");
    assert_eq!(published.digest, DIGEST);
    assert_eq!(published.physical_gpu_uuids[&0], UUID);
    assert!(!root.path().join("wrong-import").exists());
    assert!(!selected.join("__pycache__").exists());
}

fn inventory_json(devices: serde_json::Value) -> String {
    serde_json::json!({
        "schema": "capyctl-nvidia-inventory-v1",
        "host_id": "host-a",
        "digest": DIGEST,
        "devices": devices,
    })
    .to_string()
}

fn named(
    installation: &EngineInstallation,
) -> Vec<capyctl_controller::engine_provider::NamedInstallation> {
    vec![capyctl_controller::engine_provider::NamedInstallation {
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
        kv_cache_declared: false,
        deep_park: true,
        trust_remote_code: false,
        models_root: "/srv/models".into(),
        runtime_dir: "/opt/capyctl/runtime".into(),
        args: Vec::new(),
        installation_drift: Default::default(),
        cuda_home: None,
        engine_ports: (8100, 8199),
        registered: None,
    }
}

/// The boot publishes the digest, and — because the inventory holds exactly one
/// device, the shape this host's single `gpu0` policy can honestly name — the
/// device's physical UUID on the device entry.
#[test]
fn a_boot_with_an_inventory_publishes_the_digest_and_the_single_devices_uuid() {
    let published = collect_with(Path::new("/any"), None, &|_root| {
        Ok(inventory_json(serde_json::json!([
            {"physical_gpu_uuid": UUID, "pci_address": "000f:01:00.0",
             "device_minor": 0, "vendor_id": "0x10de", "device_id": "0x2e12"}
        ])))
    })
    .expect("a well-formed inventory publishes");
    assert_eq!(published.digest, DIGEST);
    assert_eq!(published.host_id, "host-a");
    assert_eq!(
        published.physical_gpu_uuids,
        BTreeMap::from([(0, UUID.to_owned())])
    );

    let host = standalone_config::host_policy(
        &named(&installation()),
        "env-1",
        1 << 40,
        Some(&published),
        &HostShape::Unified,
        capyctl_config::engine_settings::DEFAULT_ENGINE_PORTS,
    );
    assert_eq!(host["device_inventory_digest"], DIGEST);
    assert_eq!(host["name"], "host-a");
    assert_eq!(
        host["resource_policy"]["devices"]["gpu0"]["physical_gpu_uuid"],
        UUID
    );
    // The published table must actually resolve: what a deployment is qualified
    // against is the table, not the JSON fragment a test read.
    capyctl_config::effective::resolve_effective(
        &standalone_config::deployment_document(
            "m",
            "m",
            &capyctl_config::effective::ModelSource::Local {
                path: "/srv/models/m".into(),
            },
            Engine::Sglang,
            &capyctl_cli::standalone_config::TemplateMemory::Unified {
                capacity_bytes: 1 << 40,
            },
            standalone_config::DEFAULT_REQUEST_DEADLINE,
            true,
            "local",
        )
        .expect("the unified template"),
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
        let published = collect_with(Path::new("/any"), None, &|_root| match &outcome {
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
    let host = standalone_config::host_policy(
        &named(&installation()),
        "env-1",
        1 << 40,
        None,
        &HostShape::NoGpu,
        capyctl_config::engine_settings::DEFAULT_ENGINE_PORTS,
    );
    assert!(
        host["device_inventory_digest"].is_null(),
        "no inventory, no digest"
    );
    assert!(
        host["resource_policy"]["devices"]["gpu0"]["physical_gpu_uuid"].is_null(),
        "no inventory, no device UUID"
    );
}

/// Without an `nvidia-smi` sample more than one device cannot be keyed by the
/// driver index its `gpuN` entry names: the digest is published (the inventory
/// is real), no UUID is, and an SGLang deployment then fails placement closed
/// at the native gate.
#[test]
fn a_multi_device_inventory_publishes_the_digest_but_names_no_device() {
    let published = collect_with(Path::new("/any"), None, &|_root| {
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
    assert!(published.physical_gpu_uuids.is_empty());
    let host = standalone_config::host_policy(
        &named(&installation()),
        "env-1",
        1 << 40,
        Some(&published),
        &HostShape::Unified,
        capyctl_config::engine_settings::DEFAULT_ENGINE_PORTS,
    );
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

const OTHER_UUID: &str = "GPU-11111111-2222-3333-4444-555555555555";

fn two_devices() -> String {
    inventory_json(serde_json::json!([
        {"physical_gpu_uuid": UUID, "pci_address": "0000:01:00.0",
         "device_minor": 0, "vendor_id": "0x10de", "device_id": "0x2684"},
        {"physical_gpu_uuid": OTHER_UUID, "pci_address": "0000:02:00.0",
         "device_minor": 1, "vendor_id": "0x10de", "device_id": "0x2684"}
    ]))
}

/// `nvidia-smi`'s view of a device: its index, UUID and eight-digit,
/// upper-case PCI domain.
fn smi(index: u32, uuid: &str, bus: u32) -> GpuDevice {
    GpuDevice {
        index,
        uuid: uuid.into(),
        pci_bus_id: format!("00000000:{bus:02X}:00.0"),
        name: "RTX".into(),
        memory: Some(GpuMemory {
            total_bytes: 24 << 30,
            used_bytes: 0,
            free_bytes: 24 << 30,
        }),
    }
}

fn sample(devices: Vec<GpuDevice>) -> GpuSample {
    GpuSample {
        devices,
        sampled_at_ms: 1,
    }
}

// T26 (design §7): two GPUs publish both UUIDs, each keyed by the driver
// index its `gpuN` entry names, and the digest is the inventory's own.
#[test]
fn a_two_device_inventory_publishes_both_uuids_and_the_same_digest() {
    // The driver index need not follow PCI order: the key is `nvidia-smi`'s.
    let observed = sample(vec![smi(0, OTHER_UUID, 2), smi(1, UUID, 1)]);
    let published = collect_with(Path::new("/any"), Some(&observed), &|_root| {
        Ok(two_devices())
    })
    .expect("a corroborated inventory publishes");
    assert_eq!(published.digest, DIGEST);
    assert_eq!(
        published.physical_gpu_uuids,
        BTreeMap::from([(0, OTHER_UUID.to_owned()), (1, UUID.to_owned())])
    );
    let unsampled = collect_with(Path::new("/any"), None, &|_root| Ok(two_devices())).unwrap();
    assert_eq!(
        unsampled.digest, published.digest,
        "the digest does not change"
    );

    let host = standalone_config::host_policy(
        &named(&installation()),
        "env-1",
        1 << 40,
        Some(&published),
        &HostShape::Discrete(observed.devices.clone()),
        capyctl_config::engine_settings::DEFAULT_ENGINE_PORTS,
    );
    assert_eq!(host["device_inventory_digest"], DIGEST);
    let devices = &host["resource_policy"]["devices"];
    assert_eq!(devices["gpu0"]["physical_gpu_uuid"], OTHER_UUID);
    assert_eq!(devices["gpu1"]["physical_gpu_uuid"], UUID);
}

// T26 (design §1): the inventory and `nvidia-smi` must name the same UUID at
// the same PCI address, one for one; any disagreement publishes no UUIDs while
// the digest is still published.
#[test]
fn a_disagreeing_gpu_sample_publishes_no_uuids() {
    let disagreements = [
        // A UUID differs at one address.
        sample(vec![
            smi(0, UUID, 1),
            smi(1, UUID.replace('9', "8").as_str(), 2),
        ]),
        // An address the inventory did not observe.
        sample(vec![smi(0, UUID, 1), smi(1, OTHER_UUID, 3)]),
        // A device the inventory did not observe.
        sample(vec![smi(0, UUID, 1)]),
        // Swapped addresses.
        sample(vec![smi(0, UUID, 2), smi(1, OTHER_UUID, 1)]),
    ];
    for observed in disagreements {
        let published = collect_with(Path::new("/any"), Some(&observed), &|_root| {
            Ok(two_devices())
        })
        .expect("the inventory itself is well formed");
        assert_eq!(published.digest, DIGEST);
        assert!(
            published.physical_gpu_uuids.is_empty(),
            "a disagreement publishes no UUIDs: {observed:?}"
        );
    }
}

// T26: a unified host whose one device `nvidia-smi` also sees publishes the
// same `gpu0` UUID as before.
#[test]
fn a_sampled_single_device_publishes_gpu0_identically() {
    let single = || {
        Ok(inventory_json(serde_json::json!([
            {"physical_gpu_uuid": UUID, "pci_address": "000f:01:00.0",
             "device_minor": 0, "vendor_id": "0x10de", "device_id": "0x2e12"}
        ])))
    };
    let integrated = GpuDevice {
        index: 0,
        uuid: UUID.into(),
        pci_bus_id: "0000000F:01:00.0".into(),
        name: "GB10".into(),
        memory: None,
    };
    let sampled = collect_with(
        Path::new("/any"),
        Some(&sample(vec![integrated])),
        &|_root| single(),
    )
    .unwrap();
    let unsampled = collect_with(Path::new("/any"), None, &|_root| single()).unwrap();
    assert_eq!(sampled, unsampled);
    let document = |published| {
        standalone_config::host_policy(
            &named(&installation()),
            "env-1",
            1 << 40,
            Some(published),
            &HostShape::Unified,
            capyctl_config::engine_settings::DEFAULT_ENGINE_PORTS,
        )
    };
    assert_eq!(document(&sampled), document(&unsampled));
    assert_eq!(
        document(&sampled)["resource_policy"]["devices"]["gpu0"]["physical_gpu_uuid"],
        UUID
    );
}

// T26 (design §1, carried must-do): on a unified host (GB10) whose one device
// `nvidia-smi` reports at another PCI address, or under another UUID, than the
// inventory collector, no UUID is published, but the host is not broken: the
// digest is still published, its policy still resolves a deployment, and a
// vLLM launch keeps the agent's own device namespace (a lone unified device
// is never pinned), exactly as on a host that published no UUID.
#[test]
fn a_unified_host_whose_sample_disagrees_publishes_no_uuid_and_still_resolves() {
    let single = || {
        Ok(inventory_json(serde_json::json!([
            {"physical_gpu_uuid": UUID, "pci_address": "000f:01:00.0",
             "device_minor": 0, "vendor_id": "0x10de", "device_id": "0x2e12"}
        ])))
    };
    let integrated = |uuid: &str, bus: &str| GpuDevice {
        index: 0,
        uuid: uuid.into(),
        pci_bus_id: bus.into(),
        name: "GB10".into(),
        memory: None,
    };
    for disagreeing in [
        integrated(UUID, "00000000:01:00.0"),
        integrated(OTHER_UUID, "0000000F:01:00.0"),
    ] {
        let observed = sample(vec![disagreeing]);
        assert_eq!(
            capyctl_agent::gpu_memory::shape(Some(&observed)),
            Ok(HostShape::Unified)
        );
        let published = collect_with(Path::new("/any"), Some(&observed), &|_root| single())
            .expect("the inventory itself is well formed");
        assert_eq!(published.digest, DIGEST);
        assert!(published.physical_gpu_uuids.is_empty(), "{observed:?}");
        let host = standalone_config::host_policy(
            &named(&installation()),
            "env-1",
            1 << 40,
            Some(&published),
            &HostShape::Unified,
            capyctl_config::engine_settings::DEFAULT_ENGINE_PORTS,
        );
        assert_eq!(host["device_inventory_digest"], DIGEST);
        assert!(host["resource_policy"]["devices"]["gpu0"]["physical_gpu_uuid"].is_null());
        let mut vllm = installation();
        vllm.engine = Engine::Vllm;
        let effective = capyctl_config::effective::resolve_effective(
            &standalone_config::deployment_document(
                "m",
                "m",
                &capyctl_config::effective::ModelSource::Local {
                    path: "/srv/models/m".into(),
                },
                Engine::Vllm,
                &capyctl_cli::standalone_config::TemplateMemory::Unified {
                    capacity_bytes: 1 << 40,
                },
                standalone_config::DEFAULT_REQUEST_DEADLINE,
                true,
                "local",
            )
            .expect("the unified template"),
            &standalone_config::host_policy(
                &named(&vllm),
                "env-1",
                1 << 40,
                Some(&published),
                &HostShape::Unified,
                capyctl_config::engine_settings::DEFAULT_ENGINE_PORTS,
            ),
        )
        .expect("the host without a UUID still resolves");
        assert_eq!(effective.cuda_namespace(), Ok(None));
    }
}

// T14 T22 T37: remote hosts fill missing placement evidence without changing
// their enrolled name, other devices, or explicit administrator pins.
#[test]
fn remote_host_publication_preserves_aliases_and_explicit_pins() {
    let publication = capyctl_cli::device_inventory::InventoryPublication {
        host_id: "kernel-host".into(),
        digest: DIGEST.into(),
        physical_gpu_uuids: BTreeMap::from([(0, UUID.into())]),
    };
    let mut host = serde_json::json!({
        "name": "gpu-box",
        "resource_policy": {"devices": {"gpu0": {"memory_domain": "unified"}, "gpu1": {"memory_domain": "unified"}}}
    });
    let original = host.clone();
    capyctl_cli::device_inventory::publish_host(&mut host, None);
    assert_eq!(host, original, "failed observation cannot invent evidence");
    capyctl_cli::device_inventory::publish_host(&mut host, Some(&publication));
    assert_eq!(host["name"], "gpu-box");
    assert_eq!(host["device_inventory_digest"], DIGEST);
    assert_eq!(
        host["resource_policy"]["devices"]["gpu0"]["physical_gpu_uuid"],
        UUID
    );
    assert!(host["resource_policy"]["devices"]["gpu1"]
        .get("physical_gpu_uuid")
        .is_none());
    host["device_inventory_digest"] = serde_json::json!("explicit-pin");
    host["resource_policy"]["devices"]["gpu0"]["physical_gpu_uuid"] =
        serde_json::json!("explicit-uuid");
    let pinned = host.clone();
    capyctl_cli::device_inventory::publish_host(&mut host, Some(&publication));
    assert_eq!(
        host, pinned,
        "pins must be checked, never silently replaced"
    );
}

// T14 T22: adding an engine must not discard the boot's placement evidence.
// A changed explicit pin must still reach the profiles-only rejection.
#[test]
fn engine_reload_retains_boot_evidence_and_exposes_explicit_pin_changes() {
    let original = serde_json::json!({
        "name": "gpu-box", "runtime_profiles": {},
        "resource_policy": {"devices": {"gpu0": {"memory_domain": "unified"}}}
    });
    let mut running = original.clone();
    capyctl_cli::device_inventory::publish_host(
        &mut running,
        Some(&capyctl_cli::device_inventory::InventoryPublication {
            host_id: "kernel-host".into(),
            digest: DIGEST.into(),
            physical_gpu_uuids: BTreeMap::from([(0, UUID.into())]),
        }),
    );
    let mut reloaded = original;
    reloaded["runtime_profiles"] = serde_json::json!({"sglang": {"engine": "sglang"}});
    capyctl_config::engine_settings::carry_start_settings(&running, &mut reloaded);
    assert!(capyctl_config::registration::only_profiles_differ(
        &running, &reloaded
    ));
    reloaded["device_inventory_digest"] = serde_json::json!("changed-pin");
    capyctl_config::engine_settings::carry_start_settings(&running, &mut reloaded);
    assert!(!capyctl_config::registration::only_profiles_differ(
        &running, &reloaded
    ));
}
