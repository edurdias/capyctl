use capyctl_adapters::sglang::{ProtectedDescriptorFds, SglangLaunch};
use capyctl_adapters::RuntimeError;
use capyctl_domain::launch::{NativeLaunch, NativeLaunchMetadata, SglangLaunchSettings};
use serde_json::{json, Value};

const CHECKPOINT: &str = "/private/checkpoints/qwen";
const INFERENCE_REF: &str = "private://inference-reference";
const ADMIN_REF: &str = "private://admin-reference";
/// The deployment's checkpoint fingerprint; no longer a pinned revision.
const REVISION: &str = "sha256:any-model";
const RECIPE: &str = "sglang_engine_config_v2";

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
        checkpoint_revision: REVISION.into(),
        served_name: format!("route-{index}"),
        binding_id: binding,
        incarnation: "01K00000000000000000000099".into(),
        endpoint: format!("http://127.0.0.1:{}", 20000 + index),
        rendered_settings_digest: "a".repeat(64),
        placement_digest: None,
        device: capyctl_domain::launch::NativeDeviceSelection {
            host_id: "host-a".into(),
            hardware_fingerprint: "hardware-v1".into(),
            device_id: "gpu0".into(),
            memory_domain: "uma".into(),
            physical_gpu_uuid: None,
            cuda_pci_index: None,
        },
    }
}

/// ADR 0014: a deep-parking deployment stating only its KV cache.
fn settings() -> SglangLaunchSettings {
    capyctl_testkit::sglang_launch_settings()
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
            // SPEC §9.1 / T21: -B, no bytecode is written beside checked source.
            "-BIS",
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
    use std::os::unix::fs::{symlink, PermissionsExt};
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    struct Directory(std::path::PathBuf);
    impl Drop for Directory {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    let directory = Directory(
        std::path::PathBuf::from(std::env::var_os("HOME").unwrap()).join(format!(
            ".capyctl-wrapper-test-{}-{}",
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
    for mode in [0o602, 0o666] {
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(mode)).unwrap();
        assert!(matches!(render(&file), Err(RuntimeError::Unsupported)));
    }
    // T21 T37, owner decision 2026-09-23: the entry is capyctl's own helper, so
    // group write is trusted only through the owner's private group.
    let private = |_: u32, _: u32| Some(true);
    let shared = |_: u32, _: u32| Some(false);
    let undetermined = |_: u32, _: u32| None;
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o620)).unwrap();
    SglangLaunch::validate_wrapper_path_with(&file, &private).unwrap();
    for lookup in [
        &shared as &capyctl_adapters::owner_only::PrivateGroup,
        &undetermined,
    ] {
        assert!(matches!(
            SglangLaunch::validate_wrapper_path_with(&file, lookup),
            Err(RuntimeError::Unsupported)
        ));
    }
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    for mode in [0o702, 0o777] {
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(mode)).unwrap();
        assert!(matches!(render(&file), Err(RuntimeError::Unsupported)));
    }
    // An ancestor directory writable by the owner's private group is the
    // owner's; the same directory under a shared group is refused.
    std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o770)).unwrap();
    SglangLaunch::validate_wrapper_path_with(&file, &private).unwrap();
    assert!(matches!(
        SglangLaunch::validate_wrapper_path_with(&file, &shared),
        Err(RuntimeError::Unsupported)
    ));
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
    assert_eq!(a["schema_version"], 2);
    assert_eq!(a["kind"], "sglang_launch");
    // ADR 0014 §9: the retired recipe, checkpoint pin and Qwen3-4B KV bound
    // never reach the entry; ADR 0008: nor does a source revision token.
    for retired in [
        "source_revision",
        "recipe",
        "checkpoint_revision",
        "minimum_kv_bytes",
        "static_memory_fraction",
    ] {
        assert!(a.get(retired).is_none(), "{retired}");
    }
    assert_eq!(
        a["device"],
        json!({"host_id":"host-a", "hardware_fingerprint":"hardware-v1", "device_id":"gpu0", "memory_domain":"uma"})
    );
    assert_eq!(
        a["settings"],
        json!({
            "dtype": null, "quantization": null, "kv_cache_dtype": null,
            "context_length": null, "max_running_requests": null, "cuda_graphs": false,
            "language_model_only": false, "trust_remote_code": false,
            "max_total_tokens": null, "chunked_prefill_size": null,
            "tokenizer_workers": 1, "memory_saver": true, "cpu_weight_backup": false,
            "weight_restore": "disk_reload",
            "memory": {"request_bytes": 17179869184_i64, "kv_cache_bytes": 4294967296_i64,
                       "margin_bytes": 8589934592_i64, "static_bytes": 8589934592_i64},
            "extra_args": []
        })
    );
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

/// E1 / ADR 0014 §2: any model's typed settings render as the engine spells
/// them; nothing is refused for not matching a pinned checkpoint recipe.
// T14 T22
#[test]
fn typed_settings_and_extra_arguments_render_for_any_model() {
    let mut config = settings();
    config.common.dtype = Some("bfloat16".into());
    config.common.quantization = Some("modelopt_fp4".into());
    config.common.kv_cache_dtype = Some("fp8_e4m3".into());
    config.common.context_length = Some(32768);
    config.common.max_concurrent_requests = Some(16);
    config.common.cuda_graphs = Some(true);
    config.common.language_model_only = true;
    config.common.trust_remote_code = true;
    config.max_total_tokens = Some(65536);
    config.chunked_prefill_size = Some(-1);
    config.tokenizer_workers = 2;
    config.cpu_weight_backup = true;
    config.weight_restore = "cpu_backup".into();
    config.extra_args = vec!["--reasoning-parser".into(), "qwen3".into()];
    let launch = SglangLaunch::from_frozen(&frozen(metadata(1), config)).unwrap();
    let settings = &public_args(&launch)["settings"];
    assert_eq!(settings["dtype"], "bfloat16");
    assert_eq!(settings["quantization"], "modelopt_fp4");
    assert_eq!(settings["kv_cache_dtype"], "fp8_e4m3");
    assert_eq!(settings["context_length"], 32768);
    assert_eq!(settings["max_running_requests"], 16);
    assert_eq!(settings["cuda_graphs"], true);
    assert_eq!(settings["language_model_only"], true);
    assert_eq!(settings["max_total_tokens"], 65536);
    assert_eq!(settings["chunked_prefill_size"], -1);
    assert_eq!(settings["weight_restore"], "cpu_backup");
    assert_eq!(
        settings["extra_args"],
        json!(["--reasoning-parser", "qwen3"])
    );
}

/// Shapes the entry would refuse are refused before rendering; reserved
/// extra arguments are refused again here (ADR 0014 §3, §6).
// T14 T21
#[test]
fn malformed_settings_and_reserved_extra_arguments_fail_before_rendering() {
    let changes: &[fn(&mut SglangLaunchSettings)] = &[
        |s| s.common.dtype = Some("int8".into()),
        |s| s.common.quantization = Some("fp 8".into()),
        |s| s.common.kv_cache_dtype = Some(String::new()),
        |s| s.common.context_length = Some(0),
        |s| s.common.context_length = Some(u32::MAX),
        |s| s.common.max_concurrent_requests = Some(0),
        |s| s.max_total_tokens = Some(0),
        |s| s.chunked_prefill_size = Some(0),
        |s| s.chunked_prefill_size = Some(-2),
        |s| s.tokenizer_workers = 0,
        |s| s.weight_restore = "cpu_backup".into(),
        |s| s.memory.kv_cache_bytes = 0,
        // ADR 0014 §5: a KV cache larger than the whole request is refused.
        |s| s.memory.request_bytes = 2 << 30,
        |s| s.memory.margin_bytes = -1,
        |s| s.extra_args = vec!["--port".into(), "1".into()],
        |s| s.extra_args = vec!["--mem-fraction".into(), "0.9".into()],
        |s| s.extra_args = vec!["--enable-metrics".into()],
        |s| s.extra_args = vec!["--config".into(), "/tmp/c.yaml".into()],
        |s| s.extra_args = vec!["--x".into(); 257],
        |s| s.extra_args = vec!["--x\n".into()],
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

/// ADR 0014 §3: the static memory share is exact integer bytes; the entry
/// turns it into `mem_fraction_static` against the launch-time baseline.
// T14
#[test]
fn memory_request_renders_exact_bytes() {
    let mut config = settings();
    config.memory.request_bytes = i64::MAX;
    config.memory.kv_cache_bytes = i64::MAX;
    // KV equals the request: the static pool is the whole request.
    let launch = SglangLaunch::from_frozen(&frozen(metadata(1), config)).unwrap();
    let memory = &public_args(&launch)["settings"]["memory"];
    assert_eq!(memory["request_bytes"].as_i64(), Some(i64::MAX));
    assert_eq!(memory["static_bytes"].as_i64(), Some(i64::MAX));
    // Request minus margin, when it covers the declared KV cache.
    let launch = SglangLaunch::from_frozen(&frozen(metadata(1), settings())).unwrap();
    assert_eq!(
        public_args(&launch)["settings"]["memory"]["static_bytes"].as_i64(),
        Some(8 << 30)
    );
    // An explicit request below KV plus margin cannot honour the placeholder
    // margin: the static pool is the declared KV cache, within the request.
    let mut config = settings();
    config.memory.request_bytes = 8 << 30;
    let launch = SglangLaunch::from_frozen(&frozen(metadata(1), config)).unwrap();
    assert_eq!(
        public_args(&launch)["settings"]["memory"]["static_bytes"].as_i64(),
        Some(4 << 30)
    );
}

#[test]
fn ordinary_missing_and_malformed_candidate_metadata_is_rejected() {
    let changes: &[fn(&mut NativeLaunchMetadata)] = &[
        |m| m.engine = "vllm".into(),
        |m| m.recipe = "other".into(),
        |m| m.checkpoint_revision.clear(),
        |m| m.checkpoint_revision = "has space".into(),
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

/// Discrete GPU design §6 (ADR 0014 open issue 2): on a discrete device the
/// entry sizes `mem_fraction_static` against the card's total, which the
/// launching host states. The total rides the closed settings only when it is
/// known, so a unified launch renders exactly as before. The static pool is
/// the weights and the KV cache: design §3 sizes a device request as
/// `weights x 1.10 + kv`, so the margin is a tenth of the weights share, not
/// the unified placeholder that would leave the weights no room on a card.
// T26
#[test]
fn a_device_total_rides_the_settings_only_when_known() {
    let mut config = settings();
    let launch = SglangLaunch::from_frozen(&frozen(metadata(1), config.clone())).unwrap();
    let memory = public_args(&launch)["settings"]["memory"].clone();
    assert!(memory.get("device_total_bytes").is_none(), "{memory}");
    assert_eq!(memory["margin_bytes"].as_i64(), Some(8 << 30));
    config.memory.device_total_bytes = Some(16376 << 20);
    let launch = SglangLaunch::from_frozen(&frozen(metadata(1), config)).unwrap();
    let memory = public_args(&launch)["settings"]["memory"].clone();
    assert_eq!(memory["device_total_bytes"], 16376i64 << 20);
    // Request 16 GiB, KV 4 GiB: weights x 1.10 = 12 GiB, margin 12 GiB / 11.
    let margin = (12i64 << 30) / 11;
    assert_eq!(memory["margin_bytes"].as_i64(), Some(margin));
    assert_eq!(memory["static_bytes"].as_i64(), Some((16 << 30) - margin));
    // A total that is not a card's is refused, not rendered.
    let mut config = settings();
    config.memory.device_total_bytes = Some(0);
    assert!(SglangLaunch::from_frozen(&frozen(metadata(1), config)).is_err());
}
