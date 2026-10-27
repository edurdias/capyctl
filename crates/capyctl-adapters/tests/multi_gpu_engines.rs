//! ADR 0035 §§4, 9: the launch of one model on several GPUs of one host, per
//! engine: the device list in rank order (UUIDs, or indices in PCI bus order),
//! the parallel size N, the loopback pin, the reserved flags, and deep parking
//! behind the support data's gate. CPU tests only: nothing here shows an
//! engine runs on two GPUs (live rows MG1-MG14 do).

use capyctl_adapters::llamacpp::{
    engine_environment, recheck_rendered, render_command as render_llamacpp, LlamacppArgsError,
    PlanInputLlamacpp,
};
use capyctl_adapters::sglang::{frozen_from_effective, SglangLaunch};
use capyctl_adapters::vllm::{plan_from_effective, render_command};
use capyctl_config::effective::{
    resolve_effective_with_checkpoint, CheckpointFacts, CudaNamespace, EffectiveDeployment,
    Residency,
};
use capyctl_domain::launch::LaunchSettings;
use serde_json::{json, Value};

const GIB: i64 = 1 << 30;

fn uuid(n: u32) -> String {
    format!("GPU-0000000{n}-0000-0000-0000-000000000000")
}

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

/// The golden deployment on a host of `gpus` identical discrete 48 GB cards
/// (UUIDs published unless `uuids` is false), with `topology` and
/// `residency`; its phases derived from an 8 GiB checkpoint.
fn deployment(engine: &str, gpus: u32, topology: Option<u32>, uuids: bool) -> (Value, Value) {
    let (mut d, mut h) = golden(engine);
    let mut domains = json!({
        "system": {"memory": "distinct", "managed_limit": "200GiB", "free_reserve": "16GiB",
                   "parked_limit": "100GiB", "host_kv_limit": "4GiB"}
    });
    let mut devices = json!({});
    for n in 0..gpus {
        let id = format!("gpu{n}");
        domains[&id] = json!({"memory": "device", "device": id, "managed_limit": "40GiB",
            "free_reserve": "8GiB", "parked_limit": "4GiB"});
        devices[&id] = json!({"domain": id, "sharing": "shared",
                              "product_name": "NVIDIA RTX A6000"});
        if uuids {
            devices[&id]["physical_gpu_uuid"] = json!(uuid(n));
        }
    }
    h["resource_policy"]["domains"] = domains;
    h["resource_policy"]["devices"] = devices;
    let object = d.as_object_mut().unwrap();
    for field in ["resources", "residency", "devices"] {
        object.remove(field);
    }
    if let Some(n) = topology {
        object.insert("topology".into(), json!({"tensor_parallel": n}));
    }
    (d, h)
}

fn facts() -> CheckpointFacts {
    CheckpointFacts {
        weights_bytes: Some(8 * GIB),
        ..CheckpointFacts::default()
    }
}

fn resolve(engine: &str, gpus: u32, topology: Option<u32>, uuids: bool) -> EffectiveDeployment {
    let (d, h) = deployment(engine, gpus, topology, uuids);
    resolve_effective_with_checkpoint(&d, &h, facts())
        .unwrap()
        .with_device_total(|_| Some(48 * GIB))
        .unwrap()
}

fn after<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
    argv.iter()
        .position(|token| token == flag)
        .and_then(|at| argv.get(at + 1))
        .map(String::as_str)
}

// T46 T21: vLLM on two GPUs renders N with the explicit `mp` executor (never
// ray), no group flag or variable, the GPUs' UUIDs in rank order and the
// expected shape its entry checks; utilization and KV bytes come from one
// GPU's grant. One GPU renders exactly as before.
#[test]
fn vllm_renders_the_device_list_and_n() {
    let effective = resolve("vllm", 2, Some(2), true);
    let plan = plan_from_effective(&effective, 8123, "l".into(), "/r".into()).unwrap();
    assert_eq!(
        (plan.tensor_parallel_size, plan.pipeline_parallel_size),
        (2, 1)
    );
    let LaunchSettings::Vllm(settings) = &effective.engine_config else {
        panic!("vllm settings");
    };
    assert_eq!(
        plan.granted.kv_cache_bytes,
        Some(settings.memory.kv_cache_bytes)
    );
    let rendered = render_command(&plan).unwrap();
    assert_eq!(after(&rendered.argv, "--tensor-parallel-size"), Some("2"));
    assert_eq!(after(&rendered.argv, "--pipeline-parallel-size"), Some("1"));
    assert_eq!(
        after(&rendered.argv, "--distributed-executor-backend"),
        Some("mp")
    );
    for group_flag in ["--nnodes", "--node-rank", "--master-addr", "--headless"] {
        assert!(
            !rendered.argv.iter().any(|t| t == group_flag),
            "{group_flag}"
        );
    }
    assert_eq!(
        rendered.env["CUDA_VISIBLE_DEVICES"],
        format!("{},{}", uuid(0), uuid(1))
    );
    assert!(!rendered.env.contains_key("CUDA_DEVICE_ORDER"));
    assert!(!rendered.env.contains_key("CAPYCTL_GROUP_MODE"));
    let expected: Value = serde_json::from_str(&rendered.env["CAPYCTL_TP_EXPECTED"]).unwrap();
    assert_eq!(
        expected,
        json!({"tensor_parallel_size": 2, "pipeline_parallel_size": 1,
               "data_parallel_size": 1, "distributed_executor_backend": "mp",
               "devices": [uuid(0), uuid(1)], "cuda_device_order": null})
    );
    // One GPU of the same host: no executor flag, no expected payload.
    let one = resolve("vllm", 2, None, true);
    let rendered =
        render_command(&plan_from_effective(&one, 8123, "l".into(), "/r".into()).unwrap()).unwrap();
    assert_eq!(after(&rendered.argv, "--tensor-parallel-size"), Some("1"));
    assert!(!rendered
        .argv
        .iter()
        .any(|t| t == "--distributed-executor-backend"));
    assert!(!rendered.env.contains_key("CAPYCTL_TP_EXPECTED"));
    assert_eq!(rendered.env["CUDA_VISIBLE_DEVICES"], uuid(0));
}

// T46 T27 (ADR 0019 §6): without published UUIDs every GPU is pinned by its
// index in PCI bus order, on vLLM and llama.cpp alike, and the entry's
// expected shape names that order.
#[test]
fn without_uuids_the_indices_pin_the_gpus_in_pci_order() {
    let effective = resolve("vllm", 2, Some(2), false);
    assert_eq!(
        effective.cuda_namespaces().unwrap().unwrap(),
        vec![CudaNamespace::PciIndex(0), CudaNamespace::PciIndex(1)]
    );
    let rendered =
        render_command(&plan_from_effective(&effective, 8123, "l".into(), "/r".into()).unwrap())
            .unwrap();
    assert_eq!(rendered.env["CUDA_VISIBLE_DEVICES"], "0,1");
    assert_eq!(rendered.env["CUDA_DEVICE_ORDER"], "PCI_BUS_ID");
    let expected: Value = serde_json::from_str(&rendered.env["CAPYCTL_TP_EXPECTED"]).unwrap();
    assert_eq!(expected["devices"], json!(["0", "1"]));
    assert_eq!(expected["cuda_device_order"], "PCI_BUS_ID");
}

// T46 T22: SGLang on two GPUs carries every GPU in rank order in its
// descriptor (rank 0 its device) and launches with both UUIDs; one GPU's
// descriptor is unchanged; a malformed list is refused before anything runs.
#[test]
fn sglang_carries_the_ordered_device_list() {
    let frozen = |effective: &EffectiveDeployment| {
        frozen_from_effective(
            effective,
            "01K00000000000000000000001",
            "01K00000000000000000000002",
            "127.0.0.1:8123",
            "toy".into(),
            "secret://engine-key".into(),
            "secret://admin-key".into(),
        )
        .unwrap()
    };
    let effective = resolve("sglang", 3, Some(2), true);
    let launch = frozen(&effective);
    let metadata = launch.metadata();
    assert_eq!(metadata.tensor_parallel(), 2);
    assert_eq!(
        metadata
            .devices
            .iter()
            .map(|d| (d.device_id.as_str(), d.physical_gpu_uuid.clone()))
            .collect::<Vec<_>>(),
        [("gpu0", Some(uuid(0))), ("gpu1", Some(uuid(1)))]
    );
    assert_eq!(metadata.devices[0], metadata.device);
    let public = SglangLaunch::from_frozen(&launch)
        .unwrap()
        .public_metadata()
        .clone();
    assert_eq!(
        public["devices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["device_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["gpu0", "gpu1"]
    );
    // UUIDs are launch parameters, never descriptor content.
    assert!(public["devices"][0].get("physical_gpu_uuid").is_none());

    let one = frozen(&resolve("sglang", 3, None, true));
    assert!(one.metadata().devices.is_empty());
    let public = SglangLaunch::from_frozen(&one)
        .unwrap()
        .public_metadata()
        .clone();
    assert!(public.get("devices").is_none());

    // Malformed lists: rank 0 not the device, a repeated GPU, a UUID beside
    // an index, a list of one.
    let mut bad = metadata.clone();
    bad.devices.swap(0, 1);
    let mut repeated = metadata.clone();
    repeated.devices[1] = repeated.devices[0].clone();
    let mut mixed = metadata.clone();
    mixed.devices[1].physical_gpu_uuid = None;
    mixed.devices[1].cuda_pci_index = Some(1);
    let mut single = metadata.clone();
    single.devices.truncate(1);
    for (name, changed) in [
        ("rank order", bad),
        ("repeated", repeated),
        ("mixed", mixed),
        ("single", single),
    ] {
        let relaunch = capyctl_domain::launch::NativeLaunch::from_frozen_store(
            changed,
            launch.checkpoint_root().into(),
            launch.executable().into(),
            launch.inference_credential_ref().into(),
            launch.admin_credential_ref().into(),
            launch.settings().clone(),
        );
        assert!(SglangLaunch::from_frozen(&relaunch).is_err(), "{name}");
    }
}

fn llamacpp_plan() -> PlanInputLlamacpp {
    PlanInputLlamacpp {
        engine_bin: "/opt/llama.cpp/bin/llama-server".into(),
        model_file: "/models/m.gguf".into(),
        served_model_name: "m".into(),
        port: 8100,
        context_length: 4096,
        slots: 1,
        n_gpu_layers: capyctl_domain::launch::LlamacppGpuLayers::All,
        cache_type: "f16".into(),
        config_dir: "/var/lib/capyctl/engines/llamacpp/config".into(),
        cache_dir: "/var/lib/capyctl/engines/llamacpp/cache".into(),
        ..PlanInputLlamacpp::default()
    }
}

// T46 T42 (ADR 0029 §6 as amended by ADR 0035 §4): llama.cpp on two GPUs
// renders the layer split from the grants and the main GPU 0, with both GPUs
// in its namespace and NCCL on loopback through the closed environment; one
// GPU renders none of it and keeps NCCL out.
#[test]
fn llamacpp_renders_the_layer_split_on_several_gpus_only() {
    let mut input = llamacpp_plan();
    input.cuda_namespaces = vec![CudaNamespace::Uuid(uuid(0)), CudaNamespace::Uuid(uuid(1))];
    input.tensor_split = vec![1, 1];
    let rendered = render_llamacpp(&input).unwrap();
    assert_eq!(after(&rendered.argv, "--split-mode"), Some("layer"));
    assert_eq!(after(&rendered.argv, "--tensor-split"), Some("1,1"));
    assert_eq!(after(&rendered.argv, "--main-gpu"), Some("0"));
    assert_eq!(after(&rendered.argv, "--fit"), Some("off"));
    assert!(!rendered.argv.iter().any(|t| t == "--device"));
    let env = engine_environment(
        &rendered.env,
        &input,
        &|name| (name == "NCCL_SOCKET_IFNAME").then(|| "eth0".to_owned()),
        capyctl_adapters::engine_telemetry::EngineTelemetry::Off,
    );
    assert_eq!(env["NCCL_SOCKET_IFNAME"], "lo");
    assert_eq!(
        env["CUDA_VISIBLE_DEVICES"],
        format!("{},{}", uuid(0), uuid(1))
    );

    let mut one = llamacpp_plan();
    one.cuda_namespaces = vec![CudaNamespace::Uuid(uuid(0))];
    let rendered = render_llamacpp(&one).unwrap();
    for flag in ["--split-mode", "--tensor-split", "--main-gpu"] {
        assert!(!rendered.argv.iter().any(|t| t == flag), "{flag}");
    }
    assert!(!rendered.env.contains_key("NCCL_SOCKET_IFNAME"));
    let mut build_env = one.clone();
    build_env.build_env = [("RUST_LOG".to_owned(), "info".to_owned())].into();
    let env = engine_environment(
        &render_llamacpp(&build_env).unwrap().env,
        &build_env,
        &|name| (name == "NCCL_SOCKET_IFNAME").then(|| "eth0".to_owned()),
        capyctl_adapters::engine_telemetry::EngineTelemetry::Off,
    );
    assert!(!env.contains_key("NCCL_SOCKET_IFNAME"));

    // A split must name one positive share per GPU of the namespace.
    for split in [vec![1], vec![1, 0], vec![1, 1, 1]] {
        let mut bad = input.clone();
        bad.tensor_split = split.clone();
        assert_eq!(
            render_llamacpp(&bad).unwrap_err(),
            LlamacppArgsError::TensorSplit,
            "{split:?}"
        );
    }
}

// T46 T42: the split options, `--device`, and the `row` and `tensor` modes
// stay reserved: refused from a host or deployment in every spelling, and
// the rendered recheck refuses any split CapyCTL did not render.
#[test]
fn llamacpp_split_and_device_options_stay_reserved() {
    for extra in [
        vec!["--split-mode", "row"],
        vec!["--split-mode", "tensor"],
        vec!["--split-mode", "layer"],
        vec!["--split-mode=row"],
        vec!["-sm", "row"],
        vec!["--tensor-split", "3,1"],
        vec!["-ts", "1,1"],
        vec!["--main-gpu", "1"],
        vec!["-mg", "1"],
        vec!["--device", "CUDA0"],
        vec!["-dev", "CUDA0"],
    ] {
        for host_fixed in [false, true] {
            let mut input = llamacpp_plan();
            let args: Vec<String> = extra.iter().map(|t| (*t).to_owned()).collect();
            if host_fixed {
                input.engine_args = args;
            } else {
                input.extra_args = args;
            }
            assert!(render_llamacpp(&input).is_err(), "{extra:?}");
        }
    }
    let mut input = llamacpp_plan();
    input.cuda_namespaces = vec![CudaNamespace::PciIndex(0), CudaNamespace::PciIndex(1)];
    input.tensor_split = vec![1, 1];
    let argv = render_llamacpp(&input).unwrap().argv;
    assert!(recheck_rendered(&argv).is_ok());
    let replace = |from: &str, to: &str| -> Vec<String> {
        argv.iter()
            .map(|t| if t == from { to.to_owned() } else { t.clone() })
            .collect()
    };
    for changed in [replace("layer", "row"), replace("layer", "tensor")] {
        assert!(recheck_rendered(&changed).is_err());
    }
    // One split option alone, or twice, is no split CapyCTL renders.
    let at = argv.iter().position(|t| t == "--main-gpu").unwrap();
    let mut alone = argv.clone();
    alone.drain(at..at + 2);
    assert!(recheck_rendered(&alone).is_err());
    let mut twice = argv.clone();
    twice.extend(["--main-gpu".to_owned(), "0".to_owned()]);
    assert!(recheck_rendered(&twice).is_err());
}

// T46 T21 (ADR 0035 §§9, 10): qualified multi-GPU deep parking renders
// vLLM's sleep mode and SGLang's memory saver through the shipped resolver.
#[test]
fn qualified_deep_parking_renders_each_engine_controls() {
    for engine in ["vllm", "sglang"] {
        let (mut d, h) = deployment(engine, 2, Some(2), true);
        d["residency"] = json!("deep");
        let effective = resolve_effective_with_checkpoint(&d, &h, facts())
            .unwrap()
            .with_device_total(|_| Some(48 * GIB))
            .unwrap();
        assert_eq!(effective.residency, Residency::Deep);
        assert_eq!(effective.local_tensor_parallel(), 2);
        match engine {
            "vllm" => {
                let plan = plan_from_effective(&effective, 8123, "l".into(), "/r".into()).unwrap();
                let argv = render_command(&plan).unwrap().argv;
                assert!(argv.iter().any(|t| t == "--enable-sleep-mode"));
                assert_eq!(after(&argv, "--tensor-parallel-size"), Some("2"));
                assert_eq!(after(&argv, "--distributed-executor-backend"), Some("mp"));
            }
            _ => {
                let launch = frozen_from_effective(
                    &effective,
                    "01K00000000000000000000001",
                    "01K00000000000000000000002",
                    "127.0.0.1:8123",
                    "toy".into(),
                    "secret://engine-key".into(),
                    "secret://admin-key".into(),
                )
                .unwrap();
                assert!(launch.settings().memory_saver);
                assert_eq!(launch.metadata().tensor_parallel(), 2);
                let public = SglangLaunch::from_frozen(&launch)
                    .unwrap()
                    .public_metadata()
                    .clone();
                assert_eq!(public["settings"]["memory_saver"], true);
                assert_eq!(public["devices"].as_array().unwrap().len(), 2);
            }
        }
    }
}
