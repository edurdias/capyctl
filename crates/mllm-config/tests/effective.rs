use mllm_config::effective::{
    binding_fingerprint, derive_default_managed_ceiling, parse_bytes, parse_duration_ms,
    resolve_effective, DeepPark, DomainMemory, Engine, ModelSource, Residency,
};
use mllm_config::resource_controls::ResourceControls;
use mllm_config::{parse_strict, ConfigErrorCode, ConfigKind};
use mllm_domain::launch::ProfileLaunchSettings;

fn fixture() -> (serde_json::Value, serde_json::Value) {
    let all: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/f2-deployment.json")).expect("fixture JSON");
    (all["deployment"].clone(), all["host"].clone())
}

/// ADR 0011: a runtime profile carries no qualification reference. A host file
/// that still has the key is refused by the strict schema with the key named.
#[test]
fn a_runtime_profile_has_no_declared_identity_key() {
    let (deployment, mut host) = fixture();
    host["runtime_profiles"]["local"]["qualification_id"] = serde_json::json!("x");
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert!(error.to_string().contains("qualification_id"), "{error}");
    host["runtime_profiles"]["local"]
        .as_object_mut()
        .unwrap()
        .remove("qualification_id");
    resolve_effective(&deployment, &host).unwrap();
}

/// Set the topology of the lab host's only domain. Used here and by the host-check
/// tests below (ADR 0010 decision 5).
fn host_with_domain_memory(memory: &str) -> serde_json::Value {
    let (_, mut host) = fixture();
    host["resource_policy"]["domains"]["unified"]["memory"] = memory.into();
    host
}

/// A host-backed park retains weights in host RAM, which frees nothing where that
/// is the same pool the device allocates from. The host is the only party that
/// knows which it is, so it states it rather than having it guessed from a domain's
/// name or from which limits happen to be set.
#[test]
fn a_domain_declares_whether_its_memory_is_one_pool() {
    let (deployment, _) = fixture();
    for (declared, expected) in [
        ("unified", DomainMemory::Unified),
        ("distinct", DomainMemory::Distinct),
    ] {
        let host = host_with_domain_memory(declared);
        let resolved = resolve_effective(&deployment, &host).expect("valid host");
        assert_eq!(resolved.host.domains["unified"].memory, expected, "{declared}");
    }
}

/// Omitting it is a configuration error, not a default. Either default is wrong on
/// one class of hardware, and the failure it causes is silent: a park that frees
/// nothing and an eviction that does not relieve pressure.
#[test]
fn a_domain_without_declared_memory_is_rejected() {
    let (deployment, mut host) = fixture();
    host["resource_policy"]["domains"]["unified"]
        .as_object_mut()
        .expect("the domain is an object")
        .remove("memory");
    let error = resolve_effective(&deployment, &host).expect_err("must be rejected");
    assert!(format!("{error}").contains("memory"), "{error}");
}

/// SPEC §6.2 distinguishes a host-backed park, which retains a weight backup in host
/// RAM, from deep parking, which releases the weights. A single `warm` cannot say
/// which, and the two differ in wake cost by several times and in host RAM by orders
/// of magnitude, so the deployment names the one it wants.
#[test]
fn residency_names_which_park_the_deployment_asks_for() {
    for (declared, expected) in [
        ("restart_only", Residency::RestartOnly),
        ("host_backed", Residency::HostBacked),
        ("deep", Residency::Deep),
    ] {
        let (mut deployment, host) = fixture();
        deployment["residency"] = declared.into();
        // ADR 0010 decision 5 refuses host_backed on a unified domain, which is what
        // the lab host declares, so this asserts the vocabulary on a host that
        // allows every tier.
        let mut host = host;
        host["resource_policy"]["domains"]["unified"]["memory"] = "distinct".into();
        let resolved = resolve_effective(&deployment, &host)
            .unwrap_or_else(|error| panic!("{declared} must resolve: {error}"));
        assert_eq!(resolved.residency, expected, "for {declared}");
    }
}

/// A host-backed park retains weights in host RAM. Where that is the same pool the
/// device allocates from, it frees nothing: the park reports success, the memory is
/// still held, and the eviction it was meant to enable does not relieve pressure.
/// Refusing at configuration time is the only point where that is visible.
#[test]
fn a_host_backed_park_is_refused_on_a_unified_domain() {
    let (mut deployment, _) = fixture();
    deployment["residency"] = "host_backed".into();
    let host = host_with_domain_memory("unified");

    let error = resolve_effective(&deployment, &host).expect_err("must be refused");
    let text = format!("{error}");
    assert!(text.contains("unified"), "names the domain: {text}");
}

/// The same deployment is valid where the pools are distinct - that is the hardware
/// the tier exists for.
#[test]
fn a_host_backed_park_resolves_on_a_distinct_domain() {
    let (mut deployment, _) = fixture();
    deployment["residency"] = "host_backed".into();
    let host = host_with_domain_memory("distinct");
    resolve_effective(&deployment, &host).expect("host-backed is valid where pools differ");
}

/// Deep parking releases the weights, so it is valid on either topology.
#[test]
fn deep_parking_resolves_on_a_unified_domain() {
    let (mut deployment, _) = fixture();
    deployment["residency"] = "deep".into();
    let host = host_with_domain_memory("unified");
    resolve_effective(&deployment, &host).expect("deep parking releases, so it is valid");
}

/// `auto` is deliberately not adopted: choosing a tier at runtime is the fallback
/// ladder ADR 0010 rejects, and SGLang cannot implement one because its memory-saver
/// and weights-CPU-backup are startup flags.
#[test]
fn residency_auto_is_refused() {
    let (mut deployment, host) = fixture();
    deployment["residency"] = "auto".into();
    let error = resolve_effective(&deployment, &host).expect_err("auto must not resolve");
    assert_eq!(error.code, ConfigErrorCode::UnsupportedCombination, "{error:?}");
    assert_eq!(error.path, "deployment.residency", "{error:?}");
}

/// Spec §3: sleep mode stopped being a precondition of a parking residency. The
/// rendered launch derives its sleep behaviour from `enable_sleep_mode &&
/// deep_park == Enabled`, so a profile with sleep mode off describes a deployment
/// that restarts instead of parking, which is a supported configuration rather
/// than a rejected one. The switch that does refuse a parking deployment is
/// `deep_park`, asserted separately below.
#[test]
fn a_parking_vllm_profile_resolves_without_sleep_mode() {
    for residency in ["host_backed", "deep"] {
        let (mut deployment, mut host) = fixture();
        deployment["residency"] = residency.into();
        host["resource_policy"]["domains"]["unified"]["memory"] = "distinct".into();
        host["runtime_profiles"]["local"]["launch_settings"]["enable_sleep_mode"] = false.into();
        resolve_effective(&deployment, &host)
            .unwrap_or_else(|error| panic!("{residency} must resolve: {error}"));
    }
}

/// `restart_only` never parks, so it has nothing to gate on sleep mode: SPEC §6.2
/// keeps restart-only first-class even for engines without a qualified release API.
#[test]
fn restart_only_vllm_profile_is_accepted_without_sleep_mode() {
    let (mut deployment, mut host) = fixture();
    deployment["residency"] = "restart_only".into();
    host["runtime_profiles"]["local"]["launch_settings"]["enable_sleep_mode"] = false.into();
    resolve_effective(&deployment, &host).expect("restart_only does not require sleep mode");
}

#[test]
fn ordinary_engine_compatibility_goldens() {
    for engine in ["vllm", "sglang"] {
        let (deployment, mut host) = fixture();
        let profile = &mut host["runtime_profiles"]["local"];
        if engine != "vllm" {
            profile["engine"] = engine.into();
            profile["args"] = serde_json::json!([]);
            profile["security"]["admin_credential_ref"] = "secret://admin-key".into();
            profile["launch_settings"] = serde_json::json!({"engine":"sglang", "recipe":"qwen3_4b_instruct2507_tp1_dp1_bf16_disk_reload_v1", "requested_budget":{"kv_cache_bytes":"4GiB", "static_memory_fraction_bps":7500}});
        }
        let effective = resolve_effective(&deployment, &host).unwrap();
        let golden: serde_json::Value = serde_json::from_str(match engine {
            "vllm" => include_str!("fixtures/effective-vllm-golden.json"),
            _ => include_str!("fixtures/effective-sglang-golden.json"),
        })
        .unwrap();
        assert_eq!(
            serde_json::to_value(effective).unwrap(),
            golden["effective"],
            "{engine}"
        );
    }
}

#[test]
fn byte_parser_rejects_time_and_overflow() {
    assert_eq!(parse_bytes("1.5KiB").unwrap(), 1536);
    assert!(parse_bytes("1ms").is_err());
    assert!(parse_bytes("0.1B").is_err());
    assert!(parse_bytes("9223372036854775808B").is_err());
}

#[test]
fn duration_parser_is_separate_and_checked() {
    assert_eq!(parse_duration_ms("2s").unwrap(), 2_000);
    assert_eq!(parse_duration_ms("1.5m").unwrap(), 90_000);
    assert!(parse_duration_ms("1KiB").is_err());
    assert!(parse_duration_ms("0.0001s").is_err());
}

#[test]
fn resolves_complete_typed_configuration_without_claiming_qualification() {
    let (deployment, host) = fixture();
    let effective = resolve_effective(&deployment, &host).unwrap();
    assert_eq!(effective.profile.engine, Engine::Vllm);
    assert_eq!(
        effective.resources.ready.allocations[0].bytes,
        8 * 1024 * 1024 * 1024
    );
    assert_eq!(effective.host.observation_ttl_ms, 2_000);
    assert_eq!(effective.host.queue.max_pending_per_deployment, 64);
    assert!(matches!(
        effective.profile.launch_settings,
        ProfileLaunchSettings::Vllm(_)
    ));
    assert_eq!(effective.recipe_fingerprint.len(), 64);
    assert!(!serde_json::to_string(&effective)
        .unwrap()
        .contains("secret-value"));
}

#[test]
fn missing_host_bounds_use_named_product_defaults() {
    let (deployment, mut host) = fixture();
    let policy = host["resource_policy"].as_object_mut().unwrap();
    policy.remove("queue");
    policy.remove("max_parked");
    policy.remove("observation_ttl");
    policy.remove("planner_max_states");
    let effective = resolve_effective(&deployment, &host).unwrap();
    assert_eq!(effective.host.queue.max_pending_per_deployment, 64);
    assert_eq!(effective.host.queue.max_pending_total, 256);
    assert_eq!(effective.host.queue.max_buffered_bytes_total, 64 << 20);
    assert_eq!(effective.host.queue.request_deadline_ms, 600_000);
    assert_eq!(effective.host.queue.admission_window_ms, 2_000);
    assert_eq!(effective.host.observation_ttl_ms, 2_000);
    assert_eq!(effective.host.planner_max_states, 4096);
    assert_eq!(effective.host.max_parked, 16);
}

#[test]
fn host_bounds_accept_limits_and_reject_zero_or_just_over() {
    let (deployment, mut host) = fixture();
    let policy = &mut host["resource_policy"];
    policy["queue"] = serde_json::json!({
        "max_pending_per_deployment": 4096, "max_pending_total": 16384,
        "max_buffered_bytes_total": "1GiB", "request_deadline": "1h",
        "admission_window": "30s"
    });
    policy["observation_ttl"] = "10s".into();
    policy["planner_max_states"] = 65536.into();
    policy["max_parked"] = 0.into();
    assert!(resolve_effective(&deployment, &host).is_ok());

    for (pointer, value) in [
        (
            "/resource_policy/queue/max_pending_per_deployment",
            serde_json::json!(4097),
        ),
        (
            "/resource_policy/queue/max_pending_total",
            serde_json::json!(16385),
        ),
        (
            "/resource_policy/queue/max_buffered_bytes_total",
            serde_json::json!("1025MiB"),
        ),
        (
            "/resource_policy/queue/request_deadline",
            serde_json::json!("3601s"),
        ),
        (
            "/resource_policy/queue/admission_window",
            serde_json::json!("31s"),
        ),
        ("/resource_policy/observation_ttl", serde_json::json!("11s")),
        (
            "/resource_policy/planner_max_states",
            serde_json::json!(65537),
        ),
        ("/resource_policy/max_parked", serde_json::json!(17)),
    ] {
        let (deployment, mut host) = fixture();
        *host.pointer_mut(pointer).unwrap() = value;
        assert!(resolve_effective(&deployment, &host).is_err(), "{pointer}");
    }
    for pointer in [
        "/resource_policy/queue/max_pending_per_deployment",
        "/resource_policy/queue/max_pending_total",
        "/resource_policy/planner_max_states",
    ] {
        let (deployment, mut host) = fixture();
        *host.pointer_mut(pointer).unwrap() = 0.into();
        assert!(resolve_effective(&deployment, &host).is_err(), "{pointer}");
    }
    for (pointer, value) in [
        (
            "/resource_policy/queue/max_buffered_bytes_total",
            serde_json::json!("0B"),
        ),
        (
            "/resource_policy/queue/request_deadline",
            serde_json::json!("0ms"),
        ),
        (
            "/resource_policy/queue/admission_window",
            serde_json::json!("0ms"),
        ),
        ("/resource_policy/observation_ttl", serde_json::json!("0ms")),
        (
            "/resource_policy/endpoint_port_range/start",
            serde_json::json!(0),
        ),
    ] {
        let (deployment, mut host) = fixture();
        *host.pointer_mut(pointer).unwrap() = value;
        assert!(resolve_effective(&deployment, &host).is_err(), "{pointer}");
    }
}

#[test]
fn profile_revision_and_fingerprint_are_exact() {
    let (deployment, mut host) = fixture();
    host["runtime_profiles"]["local"]["revision"] = 8.into();
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert_eq!(error.code, ConfigErrorCode::UnsupportedCombination);

    let (deployment, mut host) = fixture();
    host["runtime_profiles"]["local"]["build_fingerprint"] = "changed".into();
    let changed = resolve_effective(&deployment, &host).unwrap();
    let (deployment, host) = fixture();
    let original = resolve_effective(&deployment, &host).unwrap();
    assert_ne!(
        changed.recipe_fingerprint,
        original.recipe_fingerprint
    );
}

#[test]
fn owned_launch_setting_mutations_change_recipe_identity() {
    let (deployment, host) = fixture();
    let original = resolve_effective(&deployment, &host)
        .unwrap()
        .recipe_fingerprint;
    for (pointer, value) in [
        (
            "/runtime_profiles/local/launch_settings/tensor_parallel_size",
            serde_json::json!(2),
        ),
        (
            "/runtime_profiles/local/launch_settings/pipeline_parallel_size",
            serde_json::json!(2),
        ),
        (
            "/runtime_profiles/local/launch_settings/kv_cache_dtype",
            serde_json::json!("fp8"),
        ),
        (
            "/runtime_profiles/local/launch_settings/block_size_tokens",
            serde_json::json!(32),
        ),
        (
            "/runtime_profiles/local/launch_settings/cpu_offload_bytes",
            serde_json::json!("1GiB"),
        ),
        (
            "/runtime_profiles/local/launch_settings/requested_budget/kv_cache_bytes",
            serde_json::json!("5GiB"),
        ),
        (
            "/runtime_profiles/local/launch_settings/requested_budget/swap_space_bytes",
            serde_json::json!("1GiB"),
        ),
        (
            "/runtime_profiles/local/launch_settings/requested_budget/gpu_utilization_pct",
            serde_json::json!(76),
        ),
    ] {
        let mut changed_host = host.clone();
        *changed_host.pointer_mut(pointer).unwrap() = value;
        let changed = resolve_effective(&deployment, &changed_host).unwrap();
        assert_ne!(original, changed.recipe_fingerprint, "{pointer}");
    }
    let mut restart = deployment.clone();
    restart["residency"] = "restart_only".into();
    let baseline = resolve_effective(&restart, &host)
        .unwrap()
        .recipe_fingerprint;
    let mut changed_host = host;
    changed_host["runtime_profiles"]["local"]["launch_settings"]["enable_sleep_mode"] =
        false.into();
    assert_ne!(
        baseline,
        resolve_effective(&restart, &changed_host)
            .unwrap()
            .recipe_fingerprint
    );
}

#[test]
fn qualification_dimensions_fail_to_alias() {
    let (deployment, host) = fixture();
    let original = resolve_effective(&deployment, &host)
        .unwrap()
        .recipe_fingerprint;
    for (side, pointer, value) in [
        (
            "deployment",
            "/model/path",
            serde_json::json!("/srv/models/toy-2"),
        ),
        (
            "deployment",
            "/model/content_fingerprint",
            serde_json::json!("sha256:model-2"),
        ),
        ("deployment", "/model/revision", serde_json::json!("r2")),
        ("deployment", "/recipe", serde_json::json!("recipe-2")),
        ("deployment", "/recovery", serde_json::json!("cold_restart")),
        (
            "deployment",
            "/residency",
            serde_json::json!("restart_only"),
        ),
        (
            "deployment",
            "/resources/ready/allocations/0/bytes",
            serde_json::json!("9GiB"),
        ),
        (
            "deployment",
            "/resources/parked/allocations/0/host_kv_bytes",
            serde_json::json!("1GiB"),
        ),
        ("host", "/hardware_fingerprint", serde_json::json!("hw-02")),
        (
            "host",
            "/environment_fingerprint",
            serde_json::json!("env-02"),
        ),
        (
            "host",
            "/runtime_profiles/local/executable",
            serde_json::json!("/bin/false"),
        ),
        (
            "host",
            "/runtime_profiles/local/args/1",
            serde_json::json!("8192"),
        ),
        (
            "host",
            "/runtime_profiles/local/log_policy/max_file_bytes",
            serde_json::json!("17MiB"),
        ),
        (
            "host",
            "/runtime_profiles/local/log_policy/retained_files",
            serde_json::json!(4),
        ),
    ] {
        let mut changed_deployment = deployment.clone();
        let mut changed_host = host.clone();
        let target = if side == "deployment" {
            &mut changed_deployment
        } else {
            &mut changed_host
        };
        *target.pointer_mut(pointer).unwrap() = value;
        let changed = resolve_effective(&changed_deployment, &changed_host).unwrap();
        assert_ne!(original, changed.recipe_fingerprint, "{pointer}");
    }

    let mut changed_deployment = deployment.clone();
    let mut changed_host = host.clone();
    changed_deployment["runtime_profile_revision"] = 8.into();
    changed_host["runtime_profiles"]["local"]["revision"] = 8.into();
    assert_ne!(
        original,
        resolve_effective(&changed_deployment, &changed_host)
            .unwrap()
            .recipe_fingerprint
    );

    let mut changed_deployment = deployment.clone();
    for phase in ["cold", "ready", "parking", "wake"] {
        changed_deployment["resources"][phase]["devices"][0]["sharing"] = "exclusive".into();
    }
    changed_deployment["devices"][0]["sharing"] = "exclusive".into();
    assert_ne!(
        original,
        resolve_effective(&changed_deployment, &host)
            .unwrap()
            .recipe_fingerprint
    );

    let mut auth_structure = host.clone();
    auth_structure["runtime_profiles"]["local"]["security"]["admin_credential_ref"] =
        "secret://admin".into();
    assert_ne!(
        original,
        resolve_effective(&deployment, &auth_structure)
            .unwrap()
            .recipe_fingerprint
    );

    let mut restart = deployment.clone();
    restart["residency"] = "restart_only".into();
    let mut controls = host.clone();
    controls["runtime_profiles"]["local"]["launch_settings"]["enable_sleep_mode"] = false.into();
    let enabled = resolve_effective(&restart, &controls)
        .unwrap()
        .recipe_fingerprint;
    controls["runtime_profiles"]["local"]["security"]["deep_park"] = "disabled".into();
    assert_ne!(
        enabled,
        resolve_effective(&restart, &controls)
            .unwrap()
            .recipe_fingerprint
    );
}

#[test]
fn unsupported_launch_mutations_fail_closed() {
    for (pointer, value) in [
        (
            "/runtime_profiles/local/launch_settings/tensor_parallel_size",
            serde_json::json!(0),
        ),
        (
            "/runtime_profiles/local/launch_settings/pipeline_parallel_size",
            serde_json::json!(0),
        ),
        (
            "/runtime_profiles/local/launch_settings/kv_cache_dtype",
            serde_json::json!(""),
        ),
        (
            "/runtime_profiles/local/launch_settings/block_size_tokens",
            serde_json::json!(0),
        ),
        (
            "/runtime_profiles/local/launch_settings/requested_budget/kv_cache_bytes",
            serde_json::json!("0B"),
        ),
        (
            "/runtime_profiles/local/launch_settings/requested_budget/gpu_utilization_pct",
            serde_json::json!(101),
        ),
    ] {
        let (deployment, mut host) = fixture();
        *host.pointer_mut(pointer).unwrap() = value;
        assert!(resolve_effective(&deployment, &host).is_err(), "{pointer}");
    }
    // A profile that declares one family and carries another family's launch
    // settings is refused: the block is closed per family (Spec §7).
    let (deployment, mut host) = fixture();
    host["runtime_profiles"]["local"]["launch_settings"]["engine"] = "sglang".into();
    assert!(resolve_effective(&deployment, &host).is_err());
}

#[test]
fn equivalent_byte_units_have_identical_recipe_identity() {
    let (deployment, host) = fixture();
    let original = resolve_effective(&deployment, &host).unwrap();
    let mut equivalent = host;
    equivalent["runtime_profiles"]["local"]["launch_settings"]["requested_budget"]
        ["kv_cache_bytes"] = "4096MiB".into();
    let normalized = resolve_effective(&deployment, &equivalent).unwrap();
    assert_eq!(
        original.recipe_fingerprint,
        normalized.recipe_fingerprint
    );
}

#[test]
fn equivalent_resource_units_have_identical_normalized_controls() {
    let (deployment, host) = fixture();
    let original = resolve_effective(&deployment, &host).unwrap();
    let mut equivalent = host;
    equivalent["resource_policy"]["domains"]["unified"]["managed_limit"] = "32768MiB".into();
    let normalized = resolve_effective(&deployment, &equivalent).unwrap();
    assert_eq!(
        ResourceControls::from_host(&original.host),
        ResourceControls::from_host(&normalized.host)
    );
    assert_eq!(
        original.recipe_fingerprint,
        normalized.recipe_fingerprint
    );
}

#[test]
fn parsed_host_uses_shared_resource_validation_during_resolution() {
    let (deployment, host) = fixture();
    let host_text = serde_json::to_string(&host).unwrap();
    let parsed = parse_strict(ConfigKind::Host, &host_text).unwrap();
    let effective = resolve_effective(&deployment, &parsed).unwrap();
    let controls = ResourceControls::from_host(&effective.host);
    assert_eq!(controls.domains["unified"].managed_limit, 32_i64 << 30);

    let mut invalid = host;
    let unified = invalid["resource_policy"]["domains"]
        .as_object_mut()
        .unwrap()
        .remove("unified")
        .unwrap();
    invalid["resource_policy"]["domains"][""] = unified;
    let invalid_text = serde_json::to_string(&invalid).unwrap();
    let parsed_invalid = parse_strict(ConfigKind::Host, &invalid_text).unwrap();
    assert!(resolve_effective(&deployment, &parsed_invalid).is_err());
}

#[test]
fn binding_dimensions_change_binding_not_recipe_identity() {
    let (deployment, host) = fixture();
    let qualification = resolve_effective(&deployment, &host)
        .unwrap()
        .recipe_fingerprint;
    let base = binding_fingerprint("127.0.0.1:8100", "toy", "secret://a", "inc-1");
    for changed in [
        binding_fingerprint("127.0.0.1:8101", "toy", "secret://a", "inc-1"),
        binding_fingerprint("127.0.0.1:8100", "toy-2", "secret://a", "inc-1"),
        binding_fingerprint("127.0.0.1:8100", "toy", "secret://b", "inc-1"),
        binding_fingerprint("127.0.0.1:8100", "toy", "secret://a", "inc-2"),
    ] {
        assert_ne!(base, changed);
    }
    assert_eq!(
        qualification,
        resolve_effective(&deployment, &host)
            .unwrap()
            .recipe_fingerprint
    );
}

#[test]
fn resolved_snapshot_isolated_from_later_profile_edits() {
    let (deployment, mut host) = fixture();
    let snapshot = resolve_effective(&deployment, &host).unwrap();
    let bytes = serde_json::to_vec(&snapshot).unwrap();
    host["runtime_profiles"]["local"]["build_fingerprint"] = "edited".into();
    let later = resolve_effective(&deployment, &host).unwrap();
    assert_eq!(bytes, serde_json::to_vec(&snapshot).unwrap());
    assert_ne!(
        snapshot.recipe_fingerprint,
        later.recipe_fingerprint
    );
}

#[test]
fn sglang_recipe_expands_to_exact_normalized_settings() {
    let (deployment, mut host) = fixture();
    host["runtime_profiles"]["local"]["engine"] = "sglang".into();
    host["runtime_profiles"]["local"]["args"] = serde_json::json!([]);
    host["runtime_profiles"]["local"]["security"]["admin_credential_ref"] = "secret://admin".into();
    host["runtime_profiles"]["local"]["launch_settings"] = serde_json::json!({"engine":"sglang", "recipe":"qwen3_4b_instruct2507_tp1_dp1_bf16_disk_reload_v1", "requested_budget":{"kv_cache_bytes":"4GiB", "static_memory_fraction_bps":7500}});
    let effective = resolve_effective(&deployment, &host).unwrap();
    let ProfileLaunchSettings::Sglang(settings) = effective.profile.launch_settings else {
        panic!("sglang settings")
    };
    assert_eq!(
        (
            settings.tensor_parallel_size,
            settings.data_parallel_size,
            settings.tokenizer_workers
        ),
        (1, 1, 1)
    );
    assert_eq!(
        (
            settings.model_dtype.as_str(),
            settings.context_tokens,
            settings.max_running_requests,
            settings.max_total_tokens
        ),
        ("bfloat16", 4096, 8, 4096)
    );
    assert!(settings.memory_saver);
    assert_eq!(settings.weight_restore, "disk_reload");
    assert!([
        settings.prefill_cuda_graphs,
        settings.decode_cuda_graphs,
        settings.cpu_weight_backup,
        settings.speculative_decoding,
        settings.lora,
        settings.trust_remote_code,
        settings.disaggregation,
        settings.external_cache,
        settings.cpu_kv_offload,
        settings.native_grpc,
    ]
    .into_iter()
    .all(|value| !value));
}

#[test]
fn credential_reference_values_do_not_change_recipe_identity() {
    let (deployment, host) = fixture();
    let original = resolve_effective(&deployment, &host).unwrap();
    let mut edited = host;
    edited["runtime_profiles"]["local"]["security"]["credential_ref"] = "secret://rotated".into();
    let rotated = resolve_effective(&deployment, &edited).unwrap();
    assert_eq!(
        original.recipe_fingerprint,
        rotated.recipe_fingerprint
    );
    assert_eq!(
        original.profile.security.credential_ref.as_deref(),
        Some("secret://engine-key")
    );
}

#[test]
fn unknown_capacity_disables_default_and_observation_derivation_has_provenance() {
    assert_eq!(derive_default_managed_ceiling(None).unwrap(), None);
    let value = derive_default_managed_ceiling(Some(100_i64 << 30))
        .unwrap()
        .unwrap();
    assert_eq!(value.protected_headroom_bytes, 20_i64 << 30);
    assert_eq!(value.managed_limit_bytes, 80_i64 << 30);
    assert!(value.provenance.contains("observed capacity"));
}

#[test]
fn sglang_requires_separate_admin_authority_reference() {
    let (deployment, mut host) = fixture();
    host["runtime_profiles"]["local"]["engine"] = "sglang".into();
    host["runtime_profiles"]["local"]["args"] = serde_json::json!([]);
    host["runtime_profiles"]["local"]["launch_settings"] = serde_json::json!({
        "engine": "sglang", "recipe": "qwen3_4b_instruct2507_tp1_dp1_bf16_disk_reload_v1",
        "requested_budget": {"kv_cache_bytes": "4GiB", "static_memory_fraction_bps": 7500}
    });
    assert!(resolve_effective(&deployment, &host).is_err());
    host["runtime_profiles"]["local"]["security"]["admin_credential_ref"] = "secret://admin".into();
    assert!(resolve_effective(&deployment, &host).is_ok());
}

#[test]
fn resolver_rejects_owned_or_unapproved_profile_arguments() {
    for argument in ["--api-key=secret-value", "--future-unsafe-flag"] {
        let (deployment, mut host) = fixture();
        host["runtime_profiles"]["local"]["args"] = serde_json::json!([argument]);
        assert!(resolve_effective(&deployment, &host).is_err(), "{argument}");
    }
}

#[test]
fn resolver_rejects_secret_device_and_unrecognized_environment_names() {
    for name in ["HF_TOKEN", "LD_PRELOAD", "CUDA_VISIBLE_DEVICES", "SURPRISE"] {
        let (deployment, mut host) = fixture();
        host["runtime_profiles"]["local"]["env"] = serde_json::json!({name: "secret-value"});
        assert!(resolve_effective(&deployment, &host).is_err(), "{name}");
    }
    let (deployment, mut host) = fixture();
    host["runtime_profiles"]["local"]["env"] = serde_json::json!({"RUST_LOG": "warn"});
    let changed = resolve_effective(&deployment, &host).unwrap();
    let (deployment, host) = fixture();
    let original = resolve_effective(&deployment, &host).unwrap();
    assert_ne!(
        changed.recipe_fingerprint,
        original.recipe_fingerprint
    );
}

#[test]
fn engines_without_reviewed_argument_allowlists_accept_only_empty_args() {
    let engine = "sglang";
    let (deployment, mut host) = fixture();
    host["runtime_profiles"]["local"]["engine"] = engine.into();
    host["runtime_profiles"]["local"]["args"] = serde_json::json!(["--max-model-len", "4096"]);
    host["runtime_profiles"]["local"]["launch_settings"] = serde_json::json!({"engine":"sglang", "recipe":"qwen3_4b_instruct2507_tp1_dp1_bf16_disk_reload_v1", "requested_budget":{"kv_cache_bytes":"4GiB", "static_memory_fraction_bps":7500}});
    host["runtime_profiles"]["local"]["security"]["admin_credential_ref"] = "secret://admin".into();
    assert!(resolve_effective(&deployment, &host).is_err(), "{engine}");
}

#[test]
fn unknown_profile_device_domain_and_bad_recipe_fail() {
    let (mut deployment, host) = fixture();
    deployment["runtime_profile"] = "missing".into();
    assert!(resolve_effective(&deployment, &host).is_err());

    let (mut deployment, host) = fixture();
    deployment["devices"][0]["id"] = "missing".into();
    assert!(resolve_effective(&deployment, &host).is_err());

    let (mut deployment, host) = fixture();
    deployment["resources"]["ready"]["allocations"][0]["domain"] = "missing".into();
    assert!(resolve_effective(&deployment, &host).is_err());

    let (mut deployment, host) = fixture();
    deployment["resources"]["wake"]["allocations"][0]["bytes"] = "1GiB".into();
    assert!(resolve_effective(&deployment, &host).is_err());
}

#[test]
fn strict_yaml_rejects_unknown_nested_and_wrong_scalar_type() {
    let complete_unknown = include_str!("fixtures/f2-deployment-unknown.yaml");
    let error = parse_strict(ConfigKind::Deployment, complete_unknown).unwrap_err();
    assert_eq!(error.code, ConfigErrorCode::UnknownField, "{error:?}");
    let without_unknown = complete_unknown.replace("    surprise: true\n", "");
    assert!(parse_strict(ConfigKind::Deployment, &without_unknown).is_ok());

    let wrong = "schema_version: 1\nkind: deployment\nname: d\nmodel:\n  path: /m\n  content_fingerprint: fp\n  revision: r\nroutes: route-a\nruntime_profile: p\nruntime_profile_revision: 1\nrecipe: r\nresidency: deep\nrecovery: reconcile\ndevices: []\nresources: {}\n";
    assert!(parse_strict(ConfigKind::Deployment, wrong).is_err());
}

#[test]
fn typed_decode_errors_do_not_echo_supplied_secret_scalars() {
    let (mut deployment, host) = fixture();
    deployment["runtime_profile_revision"] = "do-not-echo-secret".into();
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert_eq!(error.code, ConfigErrorCode::UnsupportedCombination);
    assert!(!error.to_string().contains("do-not-echo-secret"));

    let (mut deployment, host) = fixture();
    deployment["model"]
        .as_object_mut()
        .unwrap()
        .remove("revision");
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert_eq!(error.code, ConfigErrorCode::MissingRequired);
}

/// SGLang takes its park strategy as startup flags, so the declared tier has to
/// reach them. They were constants, which meant a deployment asking for a
/// host-backed park launched an engine that could only deep-park.
#[test]
fn sglang_launch_flags_follow_the_declared_tier() {
    let expected = [
        // (residency, memory_saver, cpu_weight_backup, weight_restore)
        ("restart_only", false, false, "disk_reload"),
        ("host_backed", true, true, "cpu_backup"),
        ("deep", true, false, "disk_reload"),
    ];
    for (declared, memory_saver, cpu_weight_backup, weight_restore) in expected {
        let (mut deployment, mut host) = fixture();
        deployment["residency"] = declared.into();
        // Same profile rewrite `ordinary_engine_compatibility_goldens` uses to point
        // the lab host at SGLang.
        let profile = &mut host["runtime_profiles"]["local"];
        profile["engine"] = "sglang".into();
        profile["args"] = serde_json::json!([]);
        profile["security"]["admin_credential_ref"] = "secret://admin-key".into();
        profile["launch_settings"] = serde_json::json!({
            "engine": "sglang",
            "recipe": "qwen3_4b_instruct2507_tp1_dp1_bf16_disk_reload_v1",
            "requested_budget": {"kv_cache_bytes": "4GiB", "static_memory_fraction_bps": 7500}
        });
        // host_backed needs a host whose pools are distinct; ADR 0010 decision 5
        // refuses it otherwise, and this test is about the flags, not the host check.
        host["resource_policy"]["domains"]["unified"]["memory"] = "distinct".into();
        let resolved = resolve_effective(&deployment, &host)
            .unwrap_or_else(|error| panic!("{declared} must resolve: {error}"));
        let ProfileLaunchSettings::Sglang(settings) = &resolved.profile.launch_settings else {
            panic!("expected SGLang launch settings for {declared}");
        };
        assert_eq!(settings.memory_saver, memory_saver, "memory_saver for {declared}");
        assert_eq!(
            settings.cpu_weight_backup, cpu_weight_backup,
            "cpu_weight_backup for {declared}"
        );
        assert_eq!(
            settings.weight_restore, weight_restore,
            "weight_restore for {declared}"
        );
    }
}

#[test]
fn strict_yaml_rejects_duplicate_nested_keys() {
    let yaml = include_str!("fixtures/f2-deployment.yaml");
    let error = parse_strict(ConfigKind::Deployment, yaml).unwrap_err();
    assert_eq!(error.code, ConfigErrorCode::DuplicateKey);
}

/// Spec §3: a host that switches deep park off must not accept a deployment that
/// asks to park. Refusing at resolution is the only place the contradiction is
/// visible; accepted, it would surface as a park that never happens under memory
/// pressure, long after the deployment was admitted.
#[test]
fn deep_park_disabled_with_parking_residency_is_refused() {
    for residency in ["host_backed", "deep"] {
        let (mut deployment, mut host) = fixture();
        deployment["residency"] = residency.into();
        host["resource_policy"]["domains"]["unified"]["memory"] = "distinct".into();
        host["runtime_profiles"]["local"]["security"]["deep_park"] = "disabled".into();
        let error = resolve_effective(&deployment, &host)
            .expect_err("a parking residency on a disabled profile is refused");
        assert_eq!(
            error.path, "runtime_profiles.security.deep_park",
            "{residency}: {error:?}"
        );
        let text = error.to_string();
        assert!(text.contains("deep_park"), "{residency}: {text}");
        assert!(text.contains("restart_only"), "{residency}: {text}");
    }
    // The same host accepts the deployment that never parks.
    let (mut deployment, mut host) = fixture();
    deployment["residency"] = "restart_only".into();
    host["runtime_profiles"]["local"]["security"]["deep_park"] = "disabled".into();
    resolve_effective(&deployment, &host).expect("restart_only does not park");
}

/// Spec §3: omitting the switch keeps parking available. A host file written
/// before the rename from `experimental_controls` must not silently lose the
/// capability it already had.
#[test]
fn deep_park_defaults_to_enabled() {
    let (deployment, mut host) = fixture();
    host["runtime_profiles"]["local"]["security"]
        .as_object_mut()
        .expect("security is an object")
        .remove("deep_park");
    let effective = resolve_effective(&deployment, &host).expect("the default resolves");
    assert_eq!(effective.profile.security.deep_park, DeepPark::Enabled);
    assert!(effective.profile.security.deep_park.is_enabled());
    assert_eq!(effective.residency, Residency::Deep);
}

/// Spec §3: `--trust-remote-code` makes the engine execute Python that arrived with
/// the checkpoint. It stays on the approved argument list, so the only thing that
/// stops it being passed by habit is the host's own switch.
#[test]
fn trust_remote_code_arg_needs_the_host_switch() {
    let (deployment, mut host) = fixture();
    host["runtime_profiles"]["local"]["args"] = serde_json::json!(["--trust-remote-code"]);
    let error =
        resolve_effective(&deployment, &host).expect_err("the flag without the switch is refused");
    assert_eq!(
        error.path, "runtime_profiles.security.trust_remote_code",
        "{error:?}"
    );

    host["runtime_profiles"]["local"]["security"]["trust_remote_code"] = true.into();
    let effective = resolve_effective(&deployment, &host).expect("the switch permits the flag");
    assert!(effective.profile.security.trust_remote_code);

    // The switch on its own changes nothing about a profile that does not pass it.
    let (deployment, mut host) = fixture();
    host["runtime_profiles"]["local"]["security"]["trust_remote_code"] = true.into();
    resolve_effective(&deployment, &host).expect("an unused switch is harmless");
}

/// Spec §7: a host declares the directory its weights live under, and a relative
/// local model path means "inside it". An absolute path is taken as written, even
/// outside the store: which directories may hold weights is the operator's
/// decision, and confining them would stop a host serving a checkpoint it has.
#[test]
fn model_store_is_required_and_local_paths_resolve_against_it() {
    let (deployment, mut host) = fixture();
    host.as_object_mut()
        .expect("host is an object")
        .remove("model_store");
    let error = resolve_effective(&deployment, &host).expect_err("the store is required");
    assert_eq!(error.code, ConfigErrorCode::MissingRequired, "{error:?}");

    let (deployment, mut host) = fixture();
    host["model_store"]["path"] = "relative/store".into();
    let error = resolve_effective(&deployment, &host).expect_err("the store must be absolute");
    assert_eq!(error.path, "host.model_store.path", "{error:?}");

    for (declared, expected) in [
        ("qwen3-4b", "/srv/models/qwen3-4b"),
        ("/anywhere/x", "/anywhere/x"),
    ] {
        let (mut deployment, host) = fixture();
        let model = deployment["model"].as_object_mut().expect("model object");
        model.remove("path");
        model.insert(
            "source".into(),
            serde_json::json!({"type": "local", "path": declared}),
        );
        let effective = resolve_effective(&deployment, &host)
            .unwrap_or_else(|error| panic!("{declared} must resolve: {error}"));
        assert_eq!(
            effective.model.source,
            ModelSource::Local {
                path: declared.into()
            },
            "{declared}"
        );
        assert_eq!(
            effective.model.resolved_path.as_deref(),
            Some(expected),
            "{declared}"
        );
        assert_eq!(
            effective.model.require_resolved_path().unwrap(),
            expected,
            "{declared}"
        );
    }
}

/// Spec §7: the resolver validates the shape of a remote source and stops. It
/// performs no fetch, so it can name no local path; a caller that needs one is
/// told so rather than handed a guessed cache directory.
#[test]
fn huggingface_and_http_sources_validate_shape_but_are_not_materializable() {
    let with_source = |source: serde_json::Value| {
        let (mut deployment, host) = fixture();
        let model = deployment["model"].as_object_mut().expect("model object");
        model.remove("path");
        model.insert("source".into(), source);
        (deployment, host)
    };
    let digest = "a".repeat(64);

    for source in [
        serde_json::json!({"type": "huggingface", "repo": "Qwen/Qwen3-4B"}),
        serde_json::json!({
            "type": "huggingface", "repo": "Qwen/Qwen3-4B",
            "revision": "main", "locked_commit": "cafe1234"
        }),
        serde_json::json!({"type": "http", "url": "https://example.test/w.tar", "sha256": digest}),
    ] {
        let (deployment, host) = with_source(source.clone());
        let effective = resolve_effective(&deployment, &host)
            .unwrap_or_else(|error| panic!("{source} must resolve: {error}"));
        assert_eq!(effective.model.resolved_path, None, "{source}");
        let error = effective
            .model
            .require_resolved_path()
            .expect_err("a remote source has no local path");
        assert_eq!(error.code, ConfigErrorCode::NotMaterializable, "{source}");
    }

    // An omitted revision is legal; an empty one is not, and neither is a plain
    // HTTP URL or a digest that is not 64 hexadecimal characters.
    for source in [
        serde_json::json!({"type": "huggingface", "repo": ""}),
        serde_json::json!({"type": "huggingface", "repo": "r", "revision": ""}),
        serde_json::json!({"type": "http", "url": "http://example.test/w.tar", "sha256": digest}),
        serde_json::json!({"type": "http", "url": "https://example.test/w.tar", "sha256": "abc"}),
        serde_json::json!({
            "type": "http", "url": "https://example.test/w.tar",
            "sha256": "z".repeat(64)
        }),
        serde_json::json!({"type": "local", "path": ""}),
        serde_json::json!({"type": "s3", "path": "/w"}),
    ] {
        let (deployment, host) = with_source(source.clone());
        assert!(
            resolve_effective(&deployment, &host).is_err(),
            "{source} must be refused"
        );
    }
}

/// Spec §3: admission reserves the Ready footprint before the engine starts, so a
/// requested KV cache larger than that reservation would hand the engine a grant
/// nothing accounted for. The overrun would otherwise appear much later, as an
/// out-of-memory kill on a deployment that had already been accepted.
#[test]
fn requested_kv_above_ready_allocation_is_refused() {
    // The lab fixture's Ready phase allocates 8GiB.
    let (deployment, mut host) = fixture();
    let budget = &mut host["runtime_profiles"]["local"]["launch_settings"]["requested_budget"];
    budget["kv_cache_bytes"] = "8GiB".into();
    resolve_effective(&deployment, &host).expect("a request equal to the allocation is accepted");

    host["runtime_profiles"]["local"]["launch_settings"]["requested_budget"]["kv_cache_bytes"] =
        "9GiB".into();
    let error = resolve_effective(&deployment, &host).expect_err("9GiB exceeds the 8GiB Ready");
    assert_eq!(
        error.path, "runtime_profiles.launch_settings.requested_budget",
        "{error:?}"
    );

    // The same bound applies to SGLang, which states its budget differently.
    let (deployment, mut host) = fixture();
    let profile = &mut host["runtime_profiles"]["local"];
    profile["engine"] = "sglang".into();
    profile["args"] = serde_json::json!([]);
    profile["security"]["admin_credential_ref"] = "secret://admin-key".into();
    profile["launch_settings"] = serde_json::json!({
        "engine": "sglang", "recipe": "qwen3_4b_instruct2507_tp1_dp1_bf16_disk_reload_v1",
        "requested_budget": {"kv_cache_bytes": "9GiB", "static_memory_fraction_bps": 7500}
    });
    assert!(resolve_effective(&deployment, &host).is_err(), "sglang");
}

/// Spec §7: `model: { path }` predates `source` and keeps working, meaning exactly
/// a local source. Stating both is refused rather than resolved by precedence,
/// because a file that says two different things about its weights is a mistake.
#[test]
fn legacy_model_path_is_a_local_source() {
    let (deployment, host) = fixture();
    let effective = resolve_effective(&deployment, &host).expect("the legacy spelling resolves");
    assert_eq!(
        effective.model.source,
        ModelSource::Local {
            path: "/srv/models/toy".into()
        }
    );
    assert_eq!(
        effective.model.resolved_path.as_deref(),
        Some("/srv/models/toy")
    );

    let (mut deployment, host) = fixture();
    deployment["model"]["source"] = serde_json::json!({"type": "local", "path": "/other"});
    let error = resolve_effective(&deployment, &host).expect_err("both spellings is a mistake");
    assert_eq!(error.path, "model", "{error:?}");

    let (mut deployment, host) = fixture();
    deployment["model"]
        .as_object_mut()
        .expect("model object")
        .remove("path");
    assert!(resolve_effective(&deployment, &host).is_err(), "neither");
}
