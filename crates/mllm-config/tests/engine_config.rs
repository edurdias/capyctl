//! ADR 0014 (owner decisions E1, P2, Q10): deployment engine configuration,
//! reserved and sensitive options, and the memory request. CPU-only tests; none
//! of this is qualification of a native engine recipe.

use mllm_config::effective::{
    decode_effective_snapshot, deployment_command_fingerprint, resolve_effective,
    resolve_effective_with_checkpoint, resolve_memory, CheckpointFacts, MemoryInputs,
    PARKED_RESIDUAL_PLACEHOLDER_BYTES, VLLM_OVERHEAD_MARGIN_BYTES,
};
use mllm_config::{parse_strict, ConfigErrorCode, ConfigKind};
use mllm_domain::launch::{LaunchSettings, SettingSource};
use serde_json::{json, Value};

const GIB: i64 = 1 << 30;

fn fixture() -> (Value, Value) {
    let all: Value =
        serde_json::from_str(include_str!("fixtures/f2-deployment.json")).expect("fixture JSON");
    (all["deployment"].clone(), all["host"].clone())
}

fn sglang_profile(host: &mut Value) {
    let profile = &mut host["runtime_profiles"]["local"];
    profile["engine"] = "sglang".into();
    profile["args"] = json!([]);
    profile["security"]["admin_credential_ref"] = "secret://admin-key".into();
}

/// A deployment with the given extra arguments accepted, on the lab host with no
/// host-fixed arguments.
fn with_extra_args(engine: &str, args: Value) -> (Value, Value) {
    let (mut deployment, mut host) = fixture();
    host["runtime_profiles"]["local"]["args"] = json!([]);
    if engine == "sglang" {
        sglang_profile(&mut host);
    }
    deployment["engine_config"]["accept_extra_args"] = true.into();
    deployment["engine_config"]["extra_args"] = args;
    (deployment, host)
}

fn extra_args_error(engine: &str, args: Value) -> String {
    let (deployment, host) = with_extra_args(engine, args.clone());
    let error = resolve_effective(&deployment, &host)
        .expect_err(&format!("{engine} {args} must be refused"));
    assert_eq!(error.path, "engine_config.extra_args", "{args}: {error}");
    error.to_string()
}

/// ADR 0014 §1, SPEC §15.3: a host document still naming the profile's old
/// tuning block is refused with an error naming where the setting lives now,
/// both through the strict YAML walk and through direct resolution.
// T03
#[test]
fn moved_launch_settings_are_refused_with_a_pointer() {
    let (deployment, mut host) = fixture();
    host["runtime_profiles"]["local"]["launch_settings"] =
        json!({"engine": "vllm", "kv_cache_dtype": "auto"});
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert_eq!(error.code, ConfigErrorCode::UnknownField);
    assert_eq!(error.path, "host.runtime_profiles.local.launch_settings");
    assert!(error.to_string().contains("engine_config"), "{error}");

    let text = serde_json::to_string(&host).unwrap();
    let error = parse_strict(ConfigKind::Host, &text).unwrap_err();
    assert_eq!(error.code, ConfigErrorCode::UnknownField);
    assert!(error.to_string().contains("engine_config"), "{error}");
}

/// ADR 0014 §2: the typed block is strictly allowlisted by the YAML walk too.
// T03 T14
#[test]
fn strict_yaml_accepts_the_typed_block_and_refuses_unknown_fields() {
    let (mut deployment, _) = fixture();
    deployment["engine_config"] = json!({
        "dtype": "bfloat16", "quantization": "modelopt_fp4", "kv_cache_dtype": "fp8_e4m3",
        "context_length": 32768, "max_concurrent_requests": 16, "cuda_graphs": true,
        "language_model_only": true, "trust_remote_code": false,
        "memory": {"request": "40GiB", "kv_cache": "8GiB"},
        "vllm": {"block_size_tokens": 16, "max_num_batched_tokens": 8192},
        "sglang": {"max_total_tokens": 65536, "chunked_prefill_size": 4096},
        "accept_extra_args": true, "extra_args": ["--reasoning-parser", "qwen3"],
    });
    parse_strict(ConfigKind::Deployment, &deployment.to_string()).unwrap();
    for (pointer, value) in [
        ("/engine_config/surprise", json!(1)),
        ("/engine_config/memory/surprise", json!("1GiB")),
        ("/engine_config/vllm/surprise", json!(1)),
    ] {
        let mut bad = deployment.clone();
        let (parent, field) = pointer.rsplit_once('/').unwrap();
        bad.pointer_mut(parent).unwrap()[field] = value;
        let error = parse_strict(ConfigKind::Deployment, &bad.to_string()).unwrap_err();
        assert_eq!(error.code, ConfigErrorCode::UnknownField, "{pointer}");
    }
    let mut bad = deployment.clone();
    bad["engine_config"]["memory"]["kv_cache"] = "8".into();
    let error = parse_strict(ConfigKind::Deployment, &bad.to_string()).unwrap_err();
    assert_eq!(error.code, ConfigErrorCode::InvalidUnit);
}

/// ADR 0014 §6 (owner decision Q10): extra arguments pass only behind the
/// deployment's explicit flag, and a host may deny them outright.
// T14 T21
#[test]
fn extra_args_need_the_flag_and_a_host_that_allows_them() {
    let (mut deployment, host) = with_extra_args("vllm", json!(["--reasoning-parser", "qwen3"]));
    let effective = resolve_effective(&deployment, &host).expect("default host allows");
    assert_eq!(
        effective.engine_config.extra_args(),
        ["--reasoning-parser", "qwen3"]
    );
    assert_eq!(
        serde_json::to_value(&effective).unwrap()["profile"]["security"]["extra_args"],
        "allowed"
    );

    deployment["engine_config"]["accept_extra_args"] = false.into();
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert!(error.to_string().contains("accept_extra_args"), "{error}");
    deployment["engine_config"]
        .as_object_mut()
        .unwrap()
        .remove("accept_extra_args");
    assert!(resolve_effective(&deployment, &host).is_err());

    let (deployment, mut host) = with_extra_args("vllm", json!(["--reasoning-parser", "qwen3"]));
    host["runtime_profiles"]["local"]["security"]["extra_args"] = "denied".into();
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert!(error.to_string().contains("denies"), "{error}");
    // Accepting nothing on a denying host is not a contradiction.
    let (mut deployment, _) = fixture();
    deployment["engine_config"]["accept_extra_args"] = true.into();
    resolve_effective(&deployment, &host).unwrap();
}

/// ADR 0014 §3: reserved settings are refused however they are spelled: exact,
/// abbreviated (both parsers accept unambiguous prefixes), negated, with an
/// inline value, as a whole reserved family, or as a configuration file.
// T14 T21
#[test]
fn reserved_options_are_refused_in_every_spelling() {
    for args in [
        json!(["--port", "9000"]),
        json!(["--po", "9000"]),
        json!(["--host=0.0.0.0"]),
        json!(["--HOST", "0.0.0.0"]),
        json!(["--api_key", "k"]),
        json!(["--no-enable-sleep-mode"]),
        json!(["--gpu-memory-util", "0.9"]),
        json!(["--kv-cache-memory-bytes", "1"]),
        json!(["--ssl-keyfile", "/k"]),
        json!(["--data-parallel-size-local", "2"]),
        json!(["--disable-log-stats"]),
        json!(["--middleware", "x"]),
        json!(["--revision", "main"]),
        json!(["--config", "/etc/vllm.yaml"]),
        json!(["--conf=/etc/vllm.yaml"]),
        // The protected entry's own `--mllm-` family (its user-args marker):
        // accepted here, the renderer refused it after the host's durable
        // attempt and the launch waited out its Initialize deadline (found
        // live, matrix M38 on host-b, 2026-09-23).
        json!(["--mllm-matrix-exits-at-once"]),
        json!(["--mllm-user-args"]),
    ] {
        let text = extra_args_error("vllm", args.clone());
        assert!(
            text.contains("reserved") || text.contains("configuration-file"),
            "{args}: {text}"
        );
    }
    for args in [
        json!(["--mem-fraction-static", "0.9"]),
        json!(["--mem-frac", "0.9"]),
        json!(["--tp", "2"]),
        json!(["--tensor-parallel-size", "2"]),
        json!(["--model-path", "/m"]),
        json!(["--enable-metrics"]),
        json!(["--enable-memory-saver"]),
        json!(["--ssl-certfile", "/c"]),
        json!(["--modelopt-export-path", "/x"]),
        json!(["--disaggregation-bootstrap-port", "1"]),
        json!(["--log-level", "debug"]),
        json!(["--config", "/etc/sglang.yaml"]),
    ] {
        let text = extra_args_error("sglang", args.clone());
        assert!(
            text.contains("reserved") || text.contains("configuration-file"),
            "{args}: {text}"
        );
    }
}

/// SPEC §8.2: rendezvous data is mllm's. Found live 2026-09-23 (M08): the
/// SGLang scheduler's torch TCP rendezvous listened on every interface, so a
/// single rank now uses a private file store and neither the rendezvous
/// address nor its port may be chosen by a deployment, even with approval.
// T21
#[test]
fn sglang_rendezvous_address_and_port_are_reserved() {
    for args in [
        json!(["--nccl-port", "29500"]),
        json!(["--nccl-port=29500"]),
        json!(["--dist-init-addr", "0.0.0.0:29500"]),
        json!(["--nccl-init-addr", "0.0.0.0:29500"]),
    ] {
        let text = extra_args_error("sglang", args.clone());
        assert!(text.contains("reserved"), "{args}: {text}");
    }
}

/// ADR 0014 §3: `--safetensors-load-strategy` is reserved only while mllm
/// renders sleep mode, which it does for a parking deployment.
// T14
#[test]
fn the_load_strategy_is_reserved_only_under_sleep_mode() {
    let (deployment, host) =
        with_extra_args("vllm", json!(["--safetensors-load-strategy", "lazy"]));
    assert!(resolve_effective(&deployment, &host).is_err());
    let (mut deployment, host) =
        with_extra_args("vllm", json!(["--safetensors-load-strategy", "lazy"]));
    deployment["residency"] = "restart_only".into();
    resolve_effective(&deployment, &host).expect("no sleep mode, no reservation");
}

/// ADR 0014 §2, §6: shape and duplicates. A typed field's native spelling is a
/// duplicate of the typed field; host-fixed and extra arguments may not both set
/// one option; short options and stray positionals are refused.
// T14
#[test]
fn duplicates_typed_spellings_and_malformed_lists_are_refused() {
    for (args, expected) in [
        (json!(["--max-model-len", "4096"]), "context_length"),
        (json!(["--dtype", "half"]), "dtype"),
        (json!(["--trust-remote-code"]), "trust_remote_code"),
        (json!(["--no-enforce-eager"]), "cuda_graphs"),
        (json!(["--block-size", "32"]), "block_size_tokens"),
        (json!(["--seed", "1", "--seed", "2"]), "duplicate"),
        (
            json!(["--enable-prefix-caching", "--no-enable-prefix-caching"]),
            "duplicate",
        ),
        (json!(["-q", "fp8"]), "short option"),
        (json!(["stray"]), "positional"),
        (json!(["--seed", "1", "2"]), "positional"),
        (json!(["--seed="]), "requires a value"),
    ] {
        let text = extra_args_error("vllm", args.clone());
        assert!(text.contains(expected), "{args}: {text}");
    }
    for (args, expected) in [
        (json!(["--context-length", "4096"]), "context_length"),
        (json!(["--disable-cuda-graph"]), "cuda_graphs"),
        (json!(["--max-total-tokens", "4096"]), "max_total_tokens"),
    ] {
        let text = extra_args_error("sglang", args.clone());
        assert!(text.contains(expected), "{args}: {text}");
    }
    // A negative number is a value, not a short option.
    let (deployment, host) =
        with_extra_args("sglang", json!(["--schedule-conservativeness", "-1"]));
    resolve_effective(&deployment, &host).expect("negative values pass");

    let (mut deployment, host) = fixture();
    deployment["engine_config"]["accept_extra_args"] = true.into();
    deployment["engine_config"]["extra_args"] = json!(["--max-model-len", "8192"]);
    // The lab profile fixes `--max-model-len` as a host-fixed argument.
    assert!(resolve_effective(&deployment, &host).is_err());
    let (mut deployment, host) = fixture();
    deployment["engine_config"]["context_length"] = 8192.into();
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert_eq!(error.path, "engine_config.context_length");
}

/// ADR 0014 §8: code loading, paths, listeners and egress need the
/// installation to approve the option by name, and a path value must lie inside
/// an approved directory. Messages name options, never values.
// T21
#[test]
fn sensitive_options_need_named_host_approval() {
    for (engine, args) in [
        ("vllm", json!(["--worker-extension-cls", "pkg.Ext"])),
        ("vllm", json!(["--tool-parser-plugin", "/srv/plugins/p.py"])),
        ("vllm", json!(["--hf-token", "hf_secret_token_value"])),
        (
            "vllm",
            json!(["--otlp-traces-endpoint", "http://collector:4317"]),
        ),
        ("vllm", json!(["--load-format", "runai_streamer"])),
        ("vllm", json!(["--download-dir", "/srv/cache"])),
        ("vllm", json!(["--future-sidecar-port", "9100"])),
        ("vllm", json!(["--future-thing-path", "/tmp/x"])),
        ("sglang", json!(["--enable-custom-logit-processor"])),
        ("sglang", json!(["--tool-server", "http://mcp"])),
        // `--nccl-port` is reserved now (T21); another listener shape.
        ("sglang", json!(["--engine-info-bootstrap-port", "29500"])),
        ("sglang", json!(["--lora-paths", "/srv/lora/a"])),
    ] {
        let text = extra_args_error(engine, args.clone());
        assert!(text.contains("approved_options"), "{args}: {text}");
        assert!(!text.contains("hf_secret_token_value"), "{text}");
        assert!(!text.contains("collector"), "{text}");
    }

    let approve = |host: &mut Value, options: Value, paths: Value| {
        let security = &mut host["runtime_profiles"]["local"]["security"];
        security["approved_options"] = options;
        security["approved_paths"] = paths;
    };
    let (deployment, mut host) = with_extra_args("vllm", json!(["--worker-extension-cls", "x"]));
    approve(&mut host, json!(["--worker-extension-cls"]), json!([]));
    resolve_effective(&deployment, &host).expect("approved by name");

    let (deployment, mut host) =
        with_extra_args("vllm", json!(["--download-dir", "/srv/cache/hf"]));
    approve(&mut host, json!(["--download-dir"]), json!([]));
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert!(error.to_string().contains("approved_paths"), "{error}");
    approve(&mut host, json!(["--download-dir"]), json!(["/srv/cache"]));
    resolve_effective(&deployment, &host).expect("inside an approved path");
    for escape in ["/srv/cache/../etc", "/srv/cachex", "relative/dir"] {
        let (deployment, _) = with_extra_args("vllm", json!([format!("--download-dir={escape}")]));
        assert!(resolve_effective(&deployment, &host).is_err(), "{escape}");
    }

    // A chat template inside the checkpoint is ordinary; outside, it is a path.
    let (deployment, host) = with_extra_args(
        "vllm",
        json!(["--chat-template", "/srv/models/toy/chat.jinja"]),
    );
    resolve_effective(&deployment, &host).expect("inside the checkpoint");
    let (deployment, host) = with_extra_args("vllm", json!(["--chat-template", "/tmp/chat.jinja"]));
    assert!(resolve_effective(&deployment, &host).is_err());

    // An ordinary option whose name prefixes a sensitive one is still ordinary.
    let (deployment, host) = with_extra_args("vllm", json!(["--reasoning-parser", "qwen3"]));
    resolve_effective(&deployment, &host).expect("ordinary");
    // Its abbreviation of the sensitive one is not.
    extra_args_error("vllm", json!(["--reasoning-parser-plug", "x"]));
}

/// ADR 0014 §8, SPEC §8.2: sensitive shapes are matched on what the option can
/// resolve to, not only on the spelling given. Listener, bind, endpoint, IP,
/// folder and JSON configuration options (a `*-config` value can name paths
/// and endpoints the checks here never see) need named approval, including
/// abbreviations of the installed parsers' own names; multi-node rendezvous
/// and the gRPC and SSL-refresh servers are mllm's, so they are reserved.
// T21 T22
#[test]
fn sensitive_shapes_and_abbreviations_need_approval() {
    for (engine, args) in [
        (
            "sglang",
            json!(["--decoupled-spec-bind", "tcp://0.0.0.0:1"]),
        ),
        (
            "sglang",
            json!(["--decoupled-spec-connect-endpoints", "tcp://x"]),
        ),
        ("sglang", json!(["--engine-info", "29500"])),
        ("sglang", json!(["--decrypted-config", "/tmp/c"])),
        (
            "sglang",
            json!(["--debug-tensor-dump-output-folder", "/tmp/d"]),
        ),
        (
            "sglang",
            json!([
                "--remote-instance-weight-loader-seed-instance-ip",
                "10.0.0.1"
            ]),
        ),
        ("sglang", json!(["--model-loader-extra-config", "{}"])),
        ("vllm", json!(["--compilation-config", "{\"level\": 3}"])),
        ("vllm", json!(["--compilation-config.level", "3"])),
        ("vllm", json!(["--future-peer-endpoints", "a,b"])),
        ("vllm", json!(["--future-peer-ip", "10.0.0.1"])),
        ("vllm", json!(["--numa-bind"])),
        ("vllm", json!(["--kv-events", "{}"])),
        // Code loading: a class, a plugin or a loader, and abbreviations.
        ("vllm", json!(["--scheduler-cls", "pkg.Scheduler"])),
        ("vllm", json!(["--scheduler-c", "pkg.Scheduler"])),
        ("vllm", json!(["--io-processor-plugin", "p"])),
        ("vllm", json!(["--io-processor", "p"])),
        ("sglang", json!(["--custom-weight-loader", "pkg.load"])),
        ("sglang", json!(["--future-thing-class", "pkg.C"])),
    ] {
        let text = extra_args_error(engine, args.clone());
        assert!(text.contains("approved_options"), "{engine} {args}: {text}");
    }
    for args in [
        json!(["--master-ad", "10.0.0.1"]),
        json!(["--master-addr", "10.0.0.1"]),
        json!(["--master-port", "29500"]),
        json!(["--nnodes", "2"]),
        json!(["--node-rank", "1"]),
        json!(["--grpc"]),
        json!(["--enable-ssl-refresh"]),
    ] {
        let text = extra_args_error("vllm", args.clone());
        assert!(text.contains("reserved"), "{args}: {text}");
    }
    // Approved by name, the configuration option passes.
    let (deployment, mut host) =
        with_extra_args("vllm", json!(["--compilation-config", "{\"level\": 3}"]));
    host["runtime_profiles"]["local"]["security"]["approved_options"] =
        json!(["--compilation-config"]);
    resolve_effective(&deployment, &host).expect("approved by name");
}

/// ADR 0014 §8: approvals are long option names and absolute directories.
// T03
#[test]
fn approval_lists_are_validated() {
    for (field, value) in [
        ("approved_options", json!(["worker-cls"])),
        ("approved_options", json!(["--worker-cls=x"])),
        ("approved_paths", json!(["relative"])),
        ("approved_paths", json!(["/srv/../etc"])),
    ] {
        let (deployment, mut host) = fixture();
        host["runtime_profiles"]["local"]["security"][field] = value.clone();
        assert!(
            resolve_effective(&deployment, &host).is_err(),
            "{field} {value}"
        );
    }
}

/// ADR 0014 §5 (owner decision P2): the memory request arithmetic.
// T14
#[test]
fn memory_request_is_declared_or_derived() {
    let margin = VLLM_OVERHEAD_MARGIN_BYTES;
    let base = MemoryInputs {
        request: None,
        kv_cache: None,
        declared_ready_total: None,
        weights: None,
        margin,
    };
    // Neither declared: refused, an engine default takes all free memory.
    let error = resolve_memory(base).unwrap_err();
    assert_eq!(error.code, ConfigErrorCode::MissingRequired);

    // Request derived from weights + KV + margin.
    let (memory, derived) = resolve_memory(MemoryInputs {
        kv_cache: Some(8 * GIB),
        weights: Some(16 * GIB),
        ..base
    })
    .unwrap();
    assert_eq!(memory.request_bytes, 16 * GIB + 8 * GIB + margin);
    assert_eq!(derived, [("memory.request", SettingSource::Derived)]);

    // KV derived from request − weights − margin.
    let (memory, derived) = resolve_memory(MemoryInputs {
        request: Some(40 * GIB),
        weights: Some(16 * GIB),
        ..base
    })
    .unwrap();
    assert_eq!(memory.kv_cache_bytes, 40 * GIB - 16 * GIB - margin);
    assert_eq!(derived, [("memory.kv_cache", SettingSource::Derived)]);

    // A derived KV that is not positive is refused.
    let error = resolve_memory(MemoryInputs {
        request: Some(20 * GIB),
        weights: Some(16 * GIB),
        ..base
    })
    .unwrap_err();
    assert_eq!(error.path, "engine_config.memory.kv_cache");

    // Derivation without the checkpoint's size is not materializable yet.
    for inputs in [
        MemoryInputs {
            kv_cache: Some(8 * GIB),
            ..base
        },
        MemoryInputs {
            request: Some(40 * GIB),
            ..base
        },
    ] {
        let error = resolve_memory(inputs).unwrap_err();
        assert_eq!(error.code, ConfigErrorCode::NotMaterializable, "{error}");
    }

    // Both declared: no weights needed; KV above the request refused.
    let (memory, derived) = resolve_memory(MemoryInputs {
        request: Some(40 * GIB),
        kv_cache: Some(8 * GIB),
        ..base
    })
    .unwrap();
    assert_eq!(
        (memory.request_bytes, memory.kv_cache_bytes),
        (40 * GIB, 8 * GIB)
    );
    assert!(derived.is_empty());
    assert!(resolve_memory(MemoryInputs {
        request: Some(8 * GIB),
        kv_cache: Some(9 * GIB),
        ..base
    })
    .is_err());
    // Weights plus KV must fit in a declared request.
    assert!(resolve_memory(MemoryInputs {
        request: Some(20 * GIB),
        kv_cache: Some(8 * GIB),
        weights: Some(16 * GIB),
        ..base
    })
    .is_err());
    // An explicit resources Ready total is the request; a different declared
    // request is a contradiction.
    let (memory, _) = resolve_memory(MemoryInputs {
        kv_cache: Some(4 * GIB),
        declared_ready_total: Some(8 * GIB),
        ..base
    })
    .unwrap();
    assert_eq!(memory.request_bytes, 8 * GIB);
    assert!(resolve_memory(MemoryInputs {
        request: Some(9 * GIB),
        kv_cache: Some(4 * GIB),
        declared_ready_total: Some(8 * GIB),
        ..base
    })
    .is_err());
    // Overflow and non-positive values are refused, never wrapped.
    for inputs in [
        MemoryInputs {
            kv_cache: Some(i64::MAX),
            weights: Some(i64::MAX),
            ..base
        },
        MemoryInputs {
            request: Some(i64::MAX),
            weights: Some(-1),
            ..base
        },
        MemoryInputs {
            request: Some(0),
            kv_cache: Some(1),
            ..base
        },
        MemoryInputs {
            request: Some(1),
            kv_cache: Some(-1),
            ..base
        },
    ] {
        assert!(resolve_memory(inputs).is_err(), "{inputs:?}");
    }
}

fn without_resources(deployment: &mut Value) {
    deployment.as_object_mut().unwrap().remove("resources");
}

/// ADR 0014 §5: with no `resources:` block the phases are derived from the
/// memory request; the effective configuration says so, and a snapshot
/// re-derives them exactly.
// T14 T08
#[test]
fn phases_derive_from_the_memory_request_and_snapshot_exactly() {
    for (residency, parked) in [
        ("deep", PARKED_RESIDUAL_PLACEHOLDER_BYTES),
        ("restart_only", 0),
    ] {
        let (mut deployment, host) = fixture();
        without_resources(&mut deployment);
        deployment["residency"] = residency.into();
        deployment["engine_config"]["memory"] = json!({"request": "12GiB", "kv_cache": "4GiB"});
        let effective = resolve_effective(&deployment, &host).unwrap();
        for phase in [
            &effective.resources.cold,
            &effective.resources.ready,
            &effective.resources.parking,
            &effective.resources.wake,
        ] {
            assert_eq!(phase.allocations[0].bytes, 12 * GIB, "{residency}");
            assert_eq!(phase.allocations[0].domain, "unified");
            assert_eq!(phase.devices.len(), 1);
        }
        assert_eq!(effective.resources.parked.allocations[0].bytes, parked);
        assert!(effective.resources.parked.devices.is_empty());
        assert_eq!(
            effective.engine_config.provenance()["resources"],
            SettingSource::Derived
        );
        let snapshot = serde_json::to_value(&effective).unwrap();
        assert_eq!(
            decode_effective_snapshot(&snapshot.to_string()).unwrap(),
            effective,
            "{residency}"
        );
        // A forged derived phase is refused: it is re-derived, not trusted.
        let mut forged = snapshot.clone();
        forged["resources"]["ready"]["allocations"][0]["bytes"] = json!(13 * GIB);
        assert!(decode_effective_snapshot(&forged.to_string()).is_err());
    }
}

/// ADR 0014 §5, §7: the checkpoint manifest's weight size lets a deployment
/// omit its request; the snapshot records the weights so it re-derives the
/// same request, and a forged weight size is refused.
// T14 T08
#[test]
fn a_request_derived_from_checkpoint_weights_round_trips() {
    let (mut deployment, host) = fixture();
    without_resources(&mut deployment);
    deployment["engine_config"]["memory"] = json!({"kv_cache": "4GiB"});
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert_eq!(error.code, ConfigErrorCode::NotMaterializable);

    let facts = CheckpointFacts {
        weights_bytes: Some(10 * GIB),
        ..Default::default()
    };
    let effective = resolve_effective_with_checkpoint(&deployment, &host, facts).unwrap();
    let memory = effective.engine_config.memory();
    assert_eq!(
        memory.request_bytes,
        10 * GIB + 4 * GIB + VLLM_OVERHEAD_MARGIN_BYTES
    );
    assert_eq!(memory.weights_bytes, Some(10 * GIB));
    assert_eq!(
        effective.resources.ready.allocations[0].bytes,
        memory.request_bytes
    );
    let snapshot = serde_json::to_value(&effective).unwrap();
    assert_eq!(
        decode_effective_snapshot(&snapshot.to_string()).unwrap(),
        effective
    );
    let mut forged = snapshot.clone();
    forged["engine_config"]["memory"]["weights_bytes"] = json!(9 * GIB);
    assert!(decode_effective_snapshot(&forged.to_string()).is_err());
}

/// T14 / ADR 0014 §4: defaults and derivations snapshot and re-derive; a
/// snapshot cannot claim an mllm default it did not get.
// T14
#[test]
fn engine_config_snapshots_revalidate_and_cannot_be_forged() {
    for engine in ["vllm", "sglang"] {
        let (mut deployment, mut host) = fixture();
        if engine == "sglang" {
            sglang_profile(&mut host);
        } else {
            host["runtime_profiles"]["local"]["args"] = json!([]);
        }
        deployment["engine_config"] = json!({
            "dtype": "bfloat16", "context_length": 8192, "language_model_only": true,
            "memory": {"kv_cache": "4GiB"}, "accept_extra_args": true,
            "extra_args": ["--seed", "7"],
        });
        let effective = resolve_effective(&deployment, &host).unwrap();
        let snapshot = serde_json::to_value(&effective).unwrap();
        assert_eq!(
            decode_effective_snapshot(&snapshot.to_string()).unwrap(),
            effective,
            "{engine}"
        );
        for (pointer, value) in [
            ("/engine_config/common/context_length", json!(4096)),
            ("/engine_config/extra_args", json!(["--seed", "8"])),
            ("/engine_config/memory/request_bytes", json!(9 * GIB)),
        ] {
            let mut forged = snapshot.clone();
            *forged.pointer_mut(pointer).unwrap() = value;
            assert!(
                decode_effective_snapshot(&forged.to_string()).is_err(),
                "{engine} {pointer}"
            );
        }
        if engine == "sglang" {
            let mut forged = snapshot.clone();
            forged["engine_config"]["tokenizer_workers"] = json!(4);
            assert!(decode_effective_snapshot(&forged.to_string()).is_err());
        }
    }
}

/// ADR 0014 §1: the engine configuration is part of what a deploy command asks
/// for, so two commands differing only there have different identities.
// T09
#[test]
fn engine_config_is_part_of_the_command_identity() {
    let (deployment, _) = fixture();
    let original = deployment_command_fingerprint(&deployment, 300_000).unwrap();
    let mut changed = deployment.clone();
    changed["engine_config"]["context_length"] = 8192.into();
    assert_ne!(
        original,
        deployment_command_fingerprint(&changed, 300_000).unwrap()
    );
    let mut equivalent = deployment.clone();
    equivalent["engine_config"]["memory"]["kv_cache"] = "4096MiB".into();
    assert_eq!(
        original,
        deployment_command_fingerprint(&equivalent, 300_000).unwrap()
    );
    let mut derived = deployment;
    without_resources(&mut derived);
    derived["engine_config"]["memory"]["request"] = "8GiB".into();
    deployment_command_fingerprint(&derived, 300_000).expect("resources may be omitted");
}

/// ADR 0014 §5: derivation needs one memory domain and a selected device.
// T14
#[test]
fn derivation_refuses_what_it_cannot_place() {
    let (mut deployment, host) = fixture();
    without_resources(&mut deployment);
    deployment["engine_config"]["memory"] = json!({"request": "8GiB", "kv_cache": "4GiB"});
    deployment["devices"] = json!([]);
    assert!(resolve_effective(&deployment, &host).is_err());
}

/// SPEC §3: vLLM's development mode is never derived for a host that opted out.
// T21
#[test]
fn sleep_mode_follows_the_host_switch() {
    let (mut deployment, mut host) = fixture();
    deployment["residency"] = "restart_only".into();
    host["runtime_profiles"]["local"]["security"]["deep_park"] = "disabled".into();
    let effective = resolve_effective(&deployment, &host).unwrap();
    let LaunchSettings::Vllm(settings) = &effective.engine_config else {
        panic!("vLLM settings");
    };
    assert!(!settings.enable_sleep_mode);
}
