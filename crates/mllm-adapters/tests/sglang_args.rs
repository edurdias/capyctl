use mllm_adapters::RuntimeError;
use mllm_adapters::sglang::{ProtectedDescriptorFds, SglangLaunch};
use mllm_domain::launch::{
    NativeLaunch, NativeLaunchMetadata, SglangLaunchSettings, SglangRequestedBudget,
};
use serde_json::{Value, json};

const CHECKPOINT: &str = "/private/checkpoints/qwen";
const INFERENCE_REF: &str = "private://inference-reference";
const ADMIN_REF: &str = "private://admin-reference";
const SOURCE: &str = "fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1";
const REVISION: &str = "cdbee75f17c01a7cc42f958dc650907174af0554";
const RECIPE: &str = "qwen3_4b_instruct2507_tp1_dp1_bf16_disk_reload_v1";

fn wrapper() -> &'static std::path::Path {
    static PATH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        std::path::Path::new("/usr/bin/true")
            .canonicalize()
            .unwrap()
    })
}

fn metadata(index: u16) -> NativeLaunchMetadata {
    let binding = format!("01K0000000000000000000{index:04}");
    NativeLaunchMetadata {
        engine: "sglang".into(),
        recipe: RECIPE.into(),
        source_revision: SOURCE.into(),
        checkpoint_revision: REVISION.into(),
        served_name: format!("route-{index}"),
        binding_id: binding,
        incarnation: "01K00000000000000000000099".into(),
        endpoint: format!("http://127.0.0.1:{}", 20000 + index),
        rendered_settings_digest: "a".repeat(64),
        placement_digest: None,
        device: mllm_domain::launch::NativeDeviceSelection {
            host_id: "host-a".into(),
            hardware_fingerprint: "hardware-v1".into(),
            device_id: "gpu0".into(),
            memory_domain: "uma".into(),
        },
    }
}

fn settings() -> SglangLaunchSettings {
    SglangLaunchSettings {
        recipe: RECIPE.into(),
        tensor_parallel_size: 1,
        data_parallel_size: 1,
        tokenizer_workers: 1,
        model_dtype: "bfloat16".into(),
        context_tokens: 4096,
        max_running_requests: 8,
        max_total_tokens: 4096,
        prefill_cuda_graphs: false,
        decode_cuda_graphs: false,
        memory_saver: true,
        cpu_weight_backup: false,
        speculative_decoding: false,
        lora: false,
        trust_remote_code: false,
        disaggregation: false,
        external_cache: false,
        cpu_kv_offload: false,
        native_grpc: false,
        weight_restore: "disk_reload".into(),
        requested_budget: SglangRequestedBudget {
            kv_cache_bytes: 4_294_967_296,
            static_memory_fraction_bps: 7500,
        },
    }
}

fn frozen(meta: NativeLaunchMetadata, config: SglangLaunchSettings) -> NativeLaunch {
    NativeLaunch::from_frozen_store(
        meta,
        CHECKPOINT.into(),
        "/opt/sglang/bin/python3".into(),
        INFERENCE_REF.into(),
        ADMIN_REF.into(),
        config,
    )
}

fn public_args(launch: &SglangLaunch) -> Value {
    let command = launch
        .render_for_launcher(
            ProtectedDescriptorFds::for_launcher(3, 4, 5).unwrap(),
            wrapper(),
        )
        .unwrap();
    assert_eq!(
        &command.argv[..4],
        [
            "/opt/sglang/bin/python3",
            "-IS",
            wrapper().to_str().unwrap(),
            "--public-settings-json"
        ]
    );
    assert_eq!(
        &command.argv[5..],
        [
            "--launch-descriptor-fd",
            "3",
            "--inference-credential-fd",
            "4",
            "--admin-credential-fd",
            "5"
        ]
    );
    assert!(command.env.is_empty());
    serde_json::from_str(&command.argv[4]).unwrap()
}

#[test]
fn wrapper_command_cannot_resolve_through_daemon_working_directory() {
    let launch = SglangLaunch::from_frozen(&frozen(metadata(1), settings())).unwrap();
    let command = launch
        .render_for_launcher(
            ProtectedDescriptorFds::for_launcher(3, 4, 5).unwrap(),
            wrapper(),
        )
        .unwrap();
    assert!(
        std::path::Path::new(&command.argv[2]).is_absolute(),
        "wrapper selected through daemon working directory"
    );
}

#[test]
fn wrapper_rejects_relative_symlink_nonregular_and_untrusted_writes() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    struct Directory(std::path::PathBuf);
    impl Drop for Directory {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    let directory = Directory(
        std::path::PathBuf::from(std::env::var_os("HOME").unwrap()).join(format!(
            ".mllm-wrapper-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )),
    );
    std::fs::create_dir(&directory.0).unwrap();
    std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
    let file = directory.0.join("sglang_entry.py");
    std::fs::write(&file, b"# Never executed by renderer tests.\n").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let launch = SglangLaunch::from_frozen(&frozen(metadata(1), settings())).unwrap();
    let render = |path: &std::path::Path| {
        launch.render_for_launcher(ProtectedDescriptorFds::for_launcher(3, 4, 5).unwrap(), path)
    };
    assert!(render(&file).is_ok());
    let link = directory.0.join("symlink.py");
    symlink(&file, &link).unwrap();
    for path in [
        std::path::Path::new("runtime/sglang_entry.py"),
        &directory.0,
        &link,
        &directory.0.join("missing.py"),
        &directory.0.join("nested/../sglang_entry.py"),
    ] {
        assert!(matches!(render(path), Err(RuntimeError::Unsupported)));
    }
    for mode in [0o620, 0o602, 0o666] {
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(mode)).unwrap();
        assert!(matches!(render(&file), Err(RuntimeError::Unsupported)));
    }
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    for mode in [0o720, 0o702, 0o777] {
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(mode)).unwrap();
        assert!(matches!(render(&file), Err(RuntimeError::Unsupported)));
    }
    std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(render(&file).is_ok());
}

#[test]
fn two_frozen_bindings_keep_distinct_endpoints_and_served_names() {
    let first = SglangLaunch::from_frozen(&frozen(metadata(1), settings())).unwrap();
    let second = SglangLaunch::from_frozen(&frozen(metadata(2), settings())).unwrap();
    let a = public_args(&first);
    let b = public_args(&second);
    assert_eq!(a["endpoint"], "http://127.0.0.1:20001");
    assert_eq!(b["endpoint"], "http://127.0.0.1:20002");
    assert_eq!(a["served_name"], "route-1");
    assert_eq!(b["served_name"], "route-2");
    assert_eq!(a["schema_version"], 1);
    assert_eq!(a["kind"], "sglang_launch");
    assert_eq!(a["source_revision"], SOURCE);
    assert_eq!(a["checkpoint_revision"], REVISION);
    assert_eq!(
        a["device"],
        json!({"host_id":"host-a", "hardware_fingerprint":"hardware-v1", "device_id":"gpu0", "memory_domain":"uma"})
    );
    assert_eq!(
        a["settings"],
        json!({
            "recipe": RECIPE,
            "tensor_parallel_size": 1, "data_parallel_size": 1, "tokenizer_workers": 1,
            "model_dtype": "bfloat16", "context_tokens": 4096,
            "max_running_requests": 8, "max_total_tokens": 4096,
            "prefill_cuda_graphs": false, "decode_cuda_graphs": false,
            "memory_saver": true, "cpu_weight_backup": false,
            "speculative_decoding": false, "lora": false, "trust_remote_code": false,
            "disaggregation": false, "external_cache": false, "cpu_kv_offload": false,
            "native_grpc": false, "weight_restore": "disk_reload",
            "requested_budget": {"kv_cache_bytes": 4294967296_i64, "static_memory_fraction_bps": 7500}
        })
    );
    assert_eq!(a["minimum_kv_bytes"], 603_979_776);
    assert_eq!(a["static_memory_fraction"], "0.7500");
}

#[test]
fn frozen_device_selection_is_required_and_never_inferred_from_gpu_zero() {
    for mutate in [
        |m: &mut NativeLaunchMetadata| m.device.host_id.clear(),
        |m: &mut NativeLaunchMetadata| m.device.hardware_fingerprint.clear(),
        |m: &mut NativeLaunchMetadata| m.device.device_id = "gpu 0".into(),
        |m: &mut NativeLaunchMetadata| m.device.memory_domain = "../uma".into(),
    ] {
        let mut m = metadata(1);
        mutate(&mut m);
        assert!(SglangLaunch::from_frozen(&frozen(m, settings())).is_err());
    }
    let mut m = metadata(1);
    m.device.device_id = "gpu7".into();
    let launch = SglangLaunch::from_frozen(&frozen(m, settings())).unwrap();
    assert_eq!(public_args(&launch)["device"]["device_id"], "gpu7");
}

#[test]
fn unsafe_settings_and_unsupported_topology_fail_before_rendering() {
    let changes: &[fn(&mut SglangLaunchSettings)] = &[
        |s| s.memory_saver = false,
        |s| s.cpu_weight_backup = true,
        |s| s.prefill_cuda_graphs = true,
        |s| s.decode_cuda_graphs = true,
        |s| s.speculative_decoding = true,
        |s| s.lora = true,
        |s| s.trust_remote_code = true,
        |s| s.disaggregation = true,
        |s| s.external_cache = true,
        |s| s.cpu_kv_offload = true,
        |s| s.native_grpc = true,
        |s| s.tensor_parallel_size = 2,
        |s| s.data_parallel_size = 2,
        |s| s.tokenizer_workers = 2,
        |s| s.model_dtype = "float16".into(),
        |s| s.recipe = "other".into(),
        |s| s.weight_restore = "cpu_backup".into(),
        |s| s.context_tokens = u32::MAX,
        |s| s.max_running_requests = u32::MAX,
        |s| s.max_total_tokens = u32::MAX,
        |s| s.context_tokens = 0,
        |s| s.max_running_requests = 0,
        |s| s.max_total_tokens = 0,
        |s| s.requested_budget.kv_cache_bytes = -1,
        |s| s.requested_budget.kv_cache_bytes = 603_979_775,
        |s| s.requested_budget.static_memory_fraction_bps = 0,
        |s| s.requested_budget.static_memory_fraction_bps = 10_001,
        |s| s.requested_budget.static_memory_fraction_bps = u16::MAX,
    ];
    for change in changes {
        let mut config = settings();
        change(&mut config);
        assert!(matches!(
            SglangLaunch::from_frozen(&frozen(metadata(1), config)),
            Err(RuntimeError::Unsupported)
        ));
    }
}

#[test]
fn exact_budget_bounds_do_not_round_or_narrow_public_values() {
    for (bps, expected) in [(1, "0.0001"), (7501, "0.7501"), (10000, "1.0000")] {
        let mut config = settings();
        config.requested_budget.static_memory_fraction_bps = bps;
        config.requested_budget.kv_cache_bytes = 603_979_776;
        let launch = SglangLaunch::from_frozen(&frozen(metadata(1), config)).unwrap();
        let args = public_args(&launch);
        assert_eq!(args["static_memory_fraction"], expected);
        assert_eq!(
            args["settings"]["requested_budget"]["kv_cache_bytes"],
            603_979_776
        );
    }
    let mut config = settings();
    config.requested_budget.kv_cache_bytes = i64::MAX;
    let launch = SglangLaunch::from_frozen(&frozen(metadata(1), config)).unwrap();
    assert_eq!(
        public_args(&launch)["settings"]["requested_budget"]["kv_cache_bytes"].as_i64(),
        Some(i64::MAX)
    );
}

#[test]
fn ordinary_missing_and_malformed_candidate_metadata_is_rejected() {
    let changes: &[fn(&mut NativeLaunchMetadata)] = &[
        |m| m.engine = "vllm".into(),
        |m| m.recipe = "other".into(),
        |m| m.source_revision = "unverified".into(),
        |m| m.checkpoint_revision = "main".into(),
        |m| m.binding_id.clear(),
        |m| m.binding_id = "ordinary-binding".into(),
        |m| m.incarnation.clear(),
        // The served name is the deployment's route token (ASCII printable,
        // 1..=256 bytes, characters 0x21..=0x7E only, mirroring the entry's
        // check in runtime/sglang_entry.py); anything else, including the
        // retired `candidate-{binding_id}` derivation's empty, whitespace,
        // oversized, and non-ASCII shapes, is refused.
        |m| m.served_name.clear(),
        |m| m.served_name = "has space".into(),
        |m| m.served_name = "tab\tname".into(),
        |m| m.served_name = "x".repeat(257),
        |m| m.served_name = "café-route".into(),
        |m| m.served_name = "route\u{202E}name".into(),
        |m| m.rendered_settings_digest = "unverified".into(),
        |m| m.endpoint = "http://0.0.0.0:20001".into(),
        |m| m.endpoint = "http://127.0.0.1:0".into(),
        |m| m.endpoint = "http://127.0.0.1:65536".into(),
        |m| m.endpoint = "http://127.0.0.1:020001".into(),
        |m| m.endpoint = "http://127.0.0.1:20001/path".into(),
        |m| m.endpoint = "http://127.0.0.1:20001?key=private".into(),
    ];
    for change in changes {
        let mut meta = metadata(1);
        change(&mut meta);
        assert!(matches!(
            SglangLaunch::from_frozen(&frozen(meta, settings())),
            Err(RuntimeError::Unsupported)
        ));
    }
}

#[test]
fn private_paths_references_and_errors_never_enter_public_command_data() {
    let launch = SglangLaunch::from_frozen(&frozen(metadata(1), settings())).unwrap();
    let command = launch
        .render_for_launcher(
            ProtectedDescriptorFds::for_launcher(7, 8, 9).unwrap(),
            wrapper(),
        )
        .unwrap();
    let outputs = [
        format!("{launch:?}"),
        format!("{launch}"),
        format!("{command:?}"),
        serde_json::to_string(launch.public_metadata()).unwrap(),
        serde_json::to_string(&json!({"argv": command.argv, "env": command.env})).unwrap(),
    ];
    for output in outputs {
        for private in [CHECKPOINT, INFERENCE_REF, ADMIN_REF] {
            assert!(!output.contains(private), "private launch data exposed");
        }
    }
    for (root, executable, inference, admin) in [
        (
            "relative/checkpoint",
            "/bin/python3",
            INFERENCE_REF,
            ADMIN_REF,
        ),
        ("/", "/bin/python3", INFERENCE_REF, ADMIN_REF),
        (
            "/models/../private",
            "/bin/python3",
            INFERENCE_REF,
            ADMIN_REF,
        ),
        (
            "/models/./private",
            "/bin/python3",
            INFERENCE_REF,
            ADMIN_REF,
        ),
        (
            "/models/line\nbreak",
            "/bin/python3",
            INFERENCE_REF,
            ADMIN_REF,
        ),
        (CHECKPOINT, "python3", INFERENCE_REF, ADMIN_REF),
        (CHECKPOINT, "/bin/python3\0suffix", INFERENCE_REF, ADMIN_REF),
        (CHECKPOINT, "/bin/python3", "", ADMIN_REF),
        (CHECKPOINT, "/bin/python3", INFERENCE_REF, ""),
        (CHECKPOINT, "/bin/python3", INFERENCE_REF, INFERENCE_REF),
        (
            CHECKPOINT,
            "/bin/python3",
            "private://with\nnewline",
            ADMIN_REF,
        ),
    ] {
        let invalid = NativeLaunch::from_frozen_store(
            metadata(1),
            root.into(),
            executable.into(),
            inference.into(),
            admin.into(),
            settings(),
        );
        let error = SglangLaunch::from_frozen(&invalid).unwrap_err();
        assert_eq!(error, RuntimeError::Unsupported);
        for private in [CHECKPOINT, INFERENCE_REF, ADMIN_REF] {
            assert!(
                !format!("{error} {error:?}").contains(private),
                "private launch data exposed"
            );
        }
    }
}

#[test]
fn launcher_descriptors_must_be_distinct_nonstandard_and_exact_native_integers() {
    for fds in [
        (-1, 4, 5),
        (0, 4, 5),
        (1, 4, 5),
        (2, 4, 5),
        (3, 3, 5),
        (3, 4, 3),
        (3, 4, 4),
        (i64::from(i32::MAX) + 1, 4, 5),
    ] {
        assert!(ProtectedDescriptorFds::for_launcher(fds.0, fds.1, fds.2).is_err());
    }
    let launch = SglangLaunch::from_frozen(&frozen(metadata(1), settings())).unwrap();
    let command = launch
        .render_for_launcher(
            ProtectedDescriptorFds::for_launcher(i64::from(i32::MAX), 4, 5).unwrap(),
            wrapper(),
        )
        .unwrap();
    assert_eq!(command.argv[6], "2147483647");
}

