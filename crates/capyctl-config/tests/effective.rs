use capyctl_config::effective::{
    binding_fingerprint, derive_default_managed_ceiling, normalize_host_policy, parse_bytes,
    parse_duration_ms, resolve_effective, resolve_effective_with_checkpoint, CheckpointFacts,
    DeepPark, DeepParkSource, DomainMemory, Engine, ModelSource, PhaseFootprint, Residency,
    ENGINE_HOST_OVERHEAD_PLACEHOLDER_BYTES, PARKED_DEVICE_RESIDUE_PLACEHOLDER_BYTES,
    PARKED_RESIDUAL_PLACEHOLDER_BYTES,
};
use capyctl_config::resource_controls::ResourceControls;
use capyctl_config::{parse_strict, ConfigErrorCode, ConfigKind};
use capyctl_domain::launch::{LaunchSettings, SettingSource};

fn fixture() -> (serde_json::Value, serde_json::Value) {
    let all: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/f2-deployment.json")).expect("fixture JSON");
    (all["deployment"].clone(), all["host"].clone())
}

/// Point the lab host's only profile at an SGLang installation. ADR 0014 §1: the
/// installation carries no tuning, so this is all an engine switch takes.
fn sglang_profile(host: &mut serde_json::Value) {
    let profile = &mut host["runtime_profiles"]["local"];
    profile["engine"] = "sglang".into();
    profile["args"] = serde_json::json!([]);
    profile["security"]["admin_credential_ref"] = "secret://admin-key".into();
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
        assert_eq!(
            resolved.host.domains["unified"].memory, expected,
            "{declared}"
        );
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
    assert!(
        error.detail.starts_with("host_backed_unavailable:"),
        "{text}"
    );
}

/// The same deployment is valid where the pools are distinct - that is the hardware
/// the tier exists for.
// T26
#[test]
fn a_host_backed_park_resolves_on_a_distinct_domain() {
    let deployment = deployment_with("host_backed", "vllm", "10GiB");
    resolve(&deployment, &discrete_host()).expect("host-backed is valid where pools differ");
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
    assert_eq!(
        error.code,
        ConfigErrorCode::UnsupportedCombination,
        "{error:?}"
    );
    assert_eq!(error.path, "deployment.residency", "{error:?}");
}

/// Spec §3: sleep mode stopped being a precondition of a parking residency. The
/// rendered launch derives its sleep behaviour from `enable_sleep_mode &&
/// deep_park == Enabled`, so a profile with sleep mode off describes a deployment
/// that restarts instead of parking, which is a supported configuration rather
/// than a rejected one. The switch that does refuse a parking deployment is
/// `deep_park`, asserted separately below.
// T14 T21
#[test]
fn a_parking_vllm_deployment_derives_sleep_mode() {
    for residency in ["host_backed", "deep"] {
        let (mut deployment, mut host) = fixture();
        deployment["residency"] = residency.into();
        host["resource_policy"]["domains"]["unified"]["memory"] = "distinct".into();
        let effective = resolve_effective(&deployment, &host)
            .unwrap_or_else(|error| panic!("{residency} must resolve: {error}"));
        let LaunchSettings::Vllm(settings) = &effective.engine_config else {
            panic!("vLLM settings");
        };
        assert!(settings.enable_sleep_mode, "{residency}");
        assert_eq!(
            settings.provenance["enable_sleep_mode"],
            SettingSource::Derived
        );
    }
}

/// `restart_only` never parks, so capyctl never renders development mode for it
/// (SPEC §6.2): the switch is derived, not declared (ADR 0014 §3).
// T14 T21
#[test]
fn restart_only_vllm_deployment_never_gets_sleep_mode() {
    let (mut deployment, host) = fixture();
    deployment["residency"] = "restart_only".into();
    let effective = resolve_effective(&deployment, &host).expect("restart_only resolves");
    let LaunchSettings::Vllm(settings) = &effective.engine_config else {
        panic!("vLLM settings");
    };
    assert!(!settings.enable_sleep_mode);
}

#[test]
fn ordinary_engine_compatibility_goldens() {
    for engine in ["vllm", "sglang"] {
        let (deployment, mut host) = fixture();
        if engine != "vllm" {
            sglang_profile(&mut host);
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
    assert!(matches!(effective.engine_config, LaunchSettings::Vllm(_)));
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
    // SPEC §10: the idle bound between a relayed stream's events.
    assert_eq!(effective.host.queue.stream_idle_ms, 120_000);
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
        "admission_window": "30s", "stream_idle_timeout": "1h"
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
    // SPEC §10: the stream idle bound is at least 1 s and at most 1 h.
    for value in ["3601s", "999ms"] {
        let (deployment, mut host) = fixture();
        host["resource_policy"]["queue"]["stream_idle_timeout"] = value.into();
        assert!(resolve_effective(&deployment, &host).is_err(), "{value}");
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
    assert_ne!(changed.recipe_fingerprint, original.recipe_fingerprint);
}

// T14
#[test]
fn engine_config_mutations_change_recipe_identity() {
    let (deployment, host) = fixture();
    let original = resolve_effective(&deployment, &host)
        .unwrap()
        .recipe_fingerprint;
    for block in [
        serde_json::json!({"memory": {"kv_cache": "5GiB"}}),
        serde_json::json!({"memory": {"kv_cache": "4GiB"}, "kv_cache_dtype": "fp8"}),
        serde_json::json!({"memory": {"kv_cache": "4GiB"}, "dtype": "bfloat16"}),
        serde_json::json!({"memory": {"kv_cache": "4GiB"}, "quantization": "modelopt_fp4"}),
        serde_json::json!({"memory": {"kv_cache": "4GiB"}, "max_concurrent_requests": 16}),
        serde_json::json!({"memory": {"kv_cache": "4GiB"}, "vllm": {"block_size_tokens": 32}}),
        serde_json::json!({"memory": {"kv_cache": "4GiB"}, "accept_extra_args": true,
            "extra_args": ["--reasoning-parser", "qwen3"]}),
    ] {
        let mut changed = deployment.clone();
        changed["engine_config"] = block.clone();
        let changed = resolve_effective(&changed, &host).unwrap();
        assert_ne!(original, changed.recipe_fingerprint, "{block}");
    }
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

// T03 T14
#[test]
fn unsupported_engine_config_fails_closed() {
    for block in [
        serde_json::json!({"memory": {"kv_cache": "0B"}}),
        serde_json::json!({"memory": {"kv_cache": "4GiB"}, "kv_cache_dtype": ""}),
        serde_json::json!({"memory": {"kv_cache": "4GiB"}, "kv_cache_dtype": "fp8 --port 1"}),
        serde_json::json!({"memory": {"kv_cache": "4GiB"}, "dtype": "fp9"}),
        serde_json::json!({"memory": {"kv_cache": "4GiB"}, "context_length": 0}),
        serde_json::json!({"memory": {"kv_cache": "4GiB"}, "max_concurrent_requests": 0}),
        serde_json::json!({"memory": {"kv_cache": "4GiB"}, "vllm": {"block_size_tokens": 0}}),
        serde_json::json!({"memory": {"kv_cache": "4GiB"}, "surprise": true}),
        serde_json::json!({"memory": {"kv_cache": "4GiB", "surprise": "1GiB"}}),
        // Closed per family (Spec §7): an SGLang block on a vLLM installation.
        serde_json::json!({"memory": {"kv_cache": "4GiB"}, "sglang": {"max_total_tokens": 4096}}),
    ] {
        let (mut deployment, host) = fixture();
        deployment["engine_config"] = block.clone();
        assert!(resolve_effective(&deployment, &host).is_err(), "{block}");
    }
}

#[test]
fn equivalent_byte_units_have_identical_recipe_identity() {
    let (deployment, host) = fixture();
    let original = resolve_effective(&deployment, &host).unwrap();
    let mut equivalent = deployment;
    equivalent["engine_config"]["memory"]["kv_cache"] = "4096MiB".into();
    let normalized = resolve_effective(&equivalent, &host).unwrap();
    assert_eq!(original.recipe_fingerprint, normalized.recipe_fingerprint);
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
    assert_eq!(original.recipe_fingerprint, normalized.recipe_fingerprint);
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
    assert_ne!(snapshot.recipe_fingerprint, later.recipe_fingerprint);
}

/// ADR 0014 §4, owner decision E1: SGLang no longer pins a recipe. Omitted
/// fields stay omitted (the engine's default applies), capyctl's safe defaults are
/// shown with their provenance, and residency-derived switches say so.
// T14
#[test]
fn sglang_settings_show_defaults_and_derivations_with_provenance() {
    let (deployment, mut host) = fixture();
    sglang_profile(&mut host);
    let effective = resolve_effective(&deployment, &host).unwrap();
    let LaunchSettings::Sglang(settings) = &effective.engine_config else {
        panic!("sglang settings")
    };
    assert_eq!(settings.common.dtype, None);
    assert_eq!(settings.common.context_length, None);
    assert_eq!(settings.max_total_tokens, None);
    assert_eq!(settings.tokenizer_workers, 1);
    assert_eq!(settings.common.cuda_graphs, Some(false));
    assert!(settings.memory_saver);
    assert!(!settings.cpu_weight_backup);
    assert_eq!(settings.weight_restore, "disk_reload");
    let provenance = &settings.provenance;
    assert_eq!(provenance["cuda_graphs"], SettingSource::CapyctlDefault);
    assert_eq!(
        provenance["sglang.tokenizer_workers"],
        SettingSource::CapyctlDefault
    );
    assert_eq!(provenance["memory_saver"], SettingSource::Derived);
    assert_eq!(provenance["memory.request"], SettingSource::Derived);
    assert!(!provenance.contains_key("memory.kv_cache"));
    let shown = serde_json::to_value(&effective).unwrap();
    assert_eq!(
        shown["engine_config"]["provenance"]["cuda_graphs"],
        "capyctl default"
    );
    assert_eq!(shown["engine_config"]["engine"], "sglang");

    // A deployment may override a safe default; the provenance entry goes away.
    let (mut deployment, _) = fixture();
    deployment["engine_config"]["cuda_graphs"] = true.into();
    deployment["engine_config"]["sglang"] = serde_json::json!({"tokenizer_workers": 2});
    let effective = resolve_effective(&deployment, &host).unwrap();
    let LaunchSettings::Sglang(settings) = &effective.engine_config else {
        panic!("sglang settings")
    };
    assert_eq!(settings.common.cuda_graphs, Some(true));
    assert_eq!(settings.tokenizer_workers, 2);
    assert!(!settings.provenance.contains_key("cuda_graphs"));
    assert!(!settings.provenance.contains_key("sglang.tokenizer_workers"));
}

/// Owner decision E1: every engine serves any model. The configuration layer
/// accepts any checkpoint shape the typed schema can express, for both engines,
/// with no recipe name and no pinned context or concurrency.
// T14 T22
#[test]
fn any_model_shape_resolves_on_either_engine() {
    for engine in ["vllm", "sglang"] {
        let (mut deployment, mut host) = fixture();
        if engine == "sglang" {
            sglang_profile(&mut host);
        } else {
            // The lab profile fixes `--max-model-len`; the typed field replaces it.
            host["runtime_profiles"]["local"]["args"] = serde_json::json!([]);
        }
        deployment["model"]["path"] = "/srv/models/qwen3.8-27b-nvfp4".into();
        deployment["recipe"] = "anything".into();
        let mut block = serde_json::json!({
            "dtype": "bfloat16", "quantization": "modelopt_fp4", "kv_cache_dtype": "fp8_e4m3",
            "context_length": 32768, "max_concurrent_requests": 16, "language_model_only": true,
            "memory": {"kv_cache": "4GiB"},
        });
        block[engine] = if engine == "vllm" {
            serde_json::json!({"block_size_tokens": 16, "max_num_batched_tokens": 8192})
        } else {
            serde_json::json!({"max_total_tokens": 65536, "chunked_prefill_size": -1})
        };
        deployment["engine_config"] = block;
        let effective = resolve_effective(&deployment, &host)
            .unwrap_or_else(|error| panic!("{engine}: {error}"));
        assert_eq!(effective.engine_config.common().context_length, Some(32768));
        assert!(effective.engine_config.common().language_model_only);
    }
}

#[test]
fn credential_reference_values_do_not_change_recipe_identity() {
    let (deployment, host) = fixture();
    let original = resolve_effective(&deployment, &host).unwrap();
    let mut edited = host;
    edited["runtime_profiles"]["local"]["security"]["credential_ref"] = "secret://rotated".into();
    let rotated = resolve_effective(&deployment, &edited).unwrap();
    assert_eq!(original.recipe_fingerprint, rotated.recipe_fingerprint);
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
    assert!(resolve_effective(&deployment, &host).is_err());
    host["runtime_profiles"]["local"]["security"]["admin_credential_ref"] = "secret://admin".into();
    assert!(resolve_effective(&deployment, &host).is_ok());
}

/// ADR 0014 §1, §3: host-fixed arguments are the installation's own. The
/// approved-flag list is gone, so an ordinary engine option passes; a reserved
/// one never does, however it is spelled.
// T03 T14
#[test]
fn resolver_rejects_reserved_profile_arguments_and_accepts_ordinary_ones() {
    for argument in [
        "--api-key=secret-value",
        "--api-k=secret-value",
        "--port",
        "--host=0.0.0.0",
        "--no-enable-sleep-mode",
        "--config=/etc/vllm.yaml",
        "-q",
    ] {
        let (deployment, mut host) = fixture();
        host["runtime_profiles"]["local"]["args"] = serde_json::json!([argument]);
        let error = resolve_effective(&deployment, &host).unwrap_err();
        assert!(!error.to_string().contains("secret-value"), "{error}");
        assert!(!error.to_string().contains("0.0.0.0"), "{error}");
    }
    let (deployment, mut host) = fixture();
    host["runtime_profiles"]["local"]["args"] =
        serde_json::json!(["--max-model-len", "4096", "--future-ordinary-flag"]);
    resolve_effective(&deployment, &host).expect("ordinary host-fixed arguments pass");
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
    assert_ne!(changed.recipe_fingerprint, original.recipe_fingerprint);
}

#[test]
fn engines_without_reviewed_argument_allowlists_accept_only_empty_args() {
    let engine = "sglang";
    let (deployment, mut host) = fixture();
    host["runtime_profiles"]["local"]["engine"] = engine.into();
    host["runtime_profiles"]["local"]["args"] = serde_json::json!(["--max-model-len", "4096"]);
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

    // Owner decision 2026-09-25: `model.revision` is defaulted now; a
    // deployment without a runtime profile is still missing one.
    let (mut deployment, host) = fixture();
    deployment
        .as_object_mut()
        .unwrap()
        .remove("runtime_profile");
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
        sglang_profile(&mut host);
        // host_backed needs a host whose pools are distinct; ADR 0010 decision 5
        // refuses it otherwise, and this test is about the flags, not the host check.
        host["resource_policy"]["domains"]["unified"]["memory"] = "distinct".into();
        let resolved = resolve_effective(&deployment, &host)
            .unwrap_or_else(|error| panic!("{declared} must resolve: {error}"));
        let LaunchSettings::Sglang(settings) = &resolved.engine_config else {
            panic!("expected SGLang launch settings for {declared}");
        };
        assert_eq!(
            settings.memory_saver, memory_saver,
            "memory_saver for {declared}"
        );
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
// T21
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

/// SPEC §9.1 / ADR 0012: deep parking is enabled unless host policy forbids it,
/// so a profile that omits the switch resolves a parking residency.
// T21
#[test]
fn deep_park_defaults_to_enabled() {
    let (mut deployment, mut host) = fixture();
    host["runtime_profiles"]["local"]["security"]
        .as_object_mut()
        .unwrap()
        .remove("deep_park");
    deployment["residency"] = "deep".into();
    let effective = resolve_effective(&deployment, &host).expect("omitted policy enables");
    assert_eq!(effective.profile.security.deep_park, DeepPark::Enabled);
    assert_eq!(effective.residency, Residency::Deep);
}

/// SPEC §9.1 / ADR 0012: `deep_park: disabled` is the host opt-out, and it is
/// honored whatever residency the deployment asks for: a parking tier is refused
/// at resolution and the restart-only tier resolves without deep parking.
// T21
#[test]
fn an_explicit_opt_out_is_honored() {
    let (mut deployment, mut host) = fixture();
    host["runtime_profiles"]["local"]["security"]["deep_park"] = "disabled".into();
    deployment["residency"] = "deep".into();
    let error = resolve_effective(&deployment, &host).expect_err("the opt-out refuses deep");
    assert_eq!(error.path, "runtime_profiles.security.deep_park");
    assert!(error.to_string().contains("opts out"), "{error}");
    deployment["residency"] = "restart_only".into();
    let effective = resolve_effective(&deployment, &host).expect("restart_only resolves");
    assert_eq!(effective.profile.security.deep_park, DeepPark::Disabled);
}

/// SPEC §7: the effective configuration says where the deep-park value came
/// from. A defaulted value is marked `default`; a value the host declared
/// carries no marker, so every explicit profile serializes exactly as before.
// T14 T21
#[test]
fn effective_configuration_shows_deep_park_provenance() {
    let (mut deployment, mut host) = fixture();
    deployment["residency"] = "restart_only".into();
    for (declared, value) in [
        ("enabled", DeepPark::Enabled),
        ("disabled", DeepPark::Disabled),
    ] {
        host["runtime_profiles"]["local"]["security"]["deep_park"] = declared.into();
        let effective = resolve_effective(&deployment, &host).unwrap();
        assert_eq!(effective.profile.security.deep_park, value);
        assert_eq!(
            effective.profile.security.deep_park_source,
            DeepParkSource::HostPolicy
        );
        let shown = serde_json::to_value(&effective).unwrap();
        assert_eq!(shown["profile"]["security"]["deep_park"], declared);
        assert!(shown["profile"]["security"]
            .get("deep_park_source")
            .is_none());
    }
    host["runtime_profiles"]["local"]["security"]
        .as_object_mut()
        .unwrap()
        .remove("deep_park");
    let effective = resolve_effective(&deployment, &host).unwrap();
    assert_eq!(
        effective.profile.security.deep_park_source,
        DeepParkSource::Default
    );
    let shown = serde_json::to_value(&effective).unwrap();
    assert_eq!(shown["profile"]["security"]["deep_park"], "enabled");
    assert_eq!(shown["profile"]["security"]["deep_park_source"], "default");
}

/// T14: provenance is derived, never declared. A host document cannot claim a
/// source for its own switch.
// T14
#[test]
fn a_host_cannot_declare_deep_park_provenance() {
    let (deployment, mut host) = fixture();
    host["runtime_profiles"]["local"]["security"]["deep_park_source"] = "host_policy".into();
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert_eq!(error.code, ConfigErrorCode::UnknownField, "{error:?}");
}

/// ADR 0014 §8: remote code is a typed field behind the host's own switch, the
/// same for both engines; host permission is necessary, not sufficient.
// T21
#[test]
fn typed_remote_code_needs_the_host_switch() {
    for engine in ["vllm", "sglang"] {
        let (mut deployment, mut host) = fixture();
        if engine == "sglang" {
            sglang_profile(&mut host);
        }
        deployment["engine_config"]["trust_remote_code"] = true.into();
        let error = resolve_effective(&deployment, &host).unwrap_err();
        assert_eq!(error.path, "engine_config.trust_remote_code", "{engine}");
        host["runtime_profiles"]["local"]["security"]["trust_remote_code"] = true.into();
        let effective = resolve_effective(&deployment, &host).unwrap();
        assert!(
            effective.engine_config.common().trust_remote_code,
            "{engine}"
        );
    }
}

/// Spec §3: `--trust-remote-code` makes the engine execute Python that arrived with
/// the checkpoint. A host-fixed argument may carry it, and the only thing that
/// stops it being passed by habit is the host's own switch.
// T21
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
// T14
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

/// ADR 0008: a remote source resolves only on a host whose policy allows its
/// kind (the default since the owner decision of 2026-09-25), only when
/// pinned (a commit SHA, a SHA-256, HTTPS), and it resolves to its fixed
/// directory in the host's sources store. The resolver performs no fetch; the
/// host materializes the directory before the first placement.
// T14
#[test]
fn remote_sources_are_allowed_by_default_need_pins_and_resolve_into_the_store() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let with_source = |source: serde_json::Value, policy: Option<serde_json::Value>| {
        let (mut deployment, mut host) = fixture();
        let model = deployment["model"].as_object_mut().expect("model object");
        model.remove("path");
        model.insert("source".into(), source);
        if let Some(policy) = policy {
            host["model_sources"] = policy;
        }
        (deployment, host)
    };
    let allowed = serde_json::json!({
        "huggingface": "allowed", "http": "allowed", "max_bytes": "100GiB"
    });
    let digest = "a".repeat(64);

    for (source, expected) in [
        (
            serde_json::json!({"type": "huggingface", "repo": "Qwen/Qwen3-4B", "revision": sha}),
            format!("/srv/models/sources/huggingface/Qwen--Qwen3-4B@{sha}"),
        ),
        (
            serde_json::json!({"huggingface": {"repo": "Qwen/Qwen3-4B", "revision": sha,
                "token_ref": "secret://hf"}}),
            format!("/srv/models/sources/huggingface/Qwen--Qwen3-4B@{sha}"),
        ),
        (
            serde_json::json!({"http": {"url": "https://example.test/w.gguf", "sha256": digest}}),
            format!("/srv/models/sources/http/{digest}"),
        ),
    ] {
        // Owner decision 2026-09-25: allowed by default; an explicit
        // `denied` (or `disabled`) keeps a host's sources off.
        let (deployment, host) = with_source(source.clone(), None);
        let effective = resolve_effective(&deployment, &host).expect("allowed by default");
        assert_eq!(
            effective.model.resolved_path.as_deref(),
            Some(expected.as_str())
        );
        for off in ["denied", "disabled"] {
            let (deployment, host) = with_source(
                source.clone(),
                Some(serde_json::json!({"huggingface": off, "http": off})),
            );
            let error = resolve_effective(&deployment, &host).expect_err("explicitly off");
            assert_eq!(error.code, ConfigErrorCode::ModelSourceDenied, "{source}");
        }
        // A stated sources store holds the download instead of the model store.
        let (deployment, host) = with_source(
            source.clone(),
            Some(serde_json::json!({"path": "/state/models"})),
        );
        let effective = resolve_effective(&deployment, &host).unwrap();
        let relocated = expected.replace("/srv/models/", "/state/models/");
        assert_eq!(
            effective.model.resolved_path.as_deref(),
            Some(relocated.as_str())
        );
        assert_eq!(
            effective.checkpoint_store(),
            std::path::Path::new("/state/models")
        );
        let text = serde_json::to_string(&effective).unwrap();
        assert_eq!(
            capyctl_config::effective::decode_effective_snapshot(&text).unwrap(),
            effective,
            "{source}"
        );

        let (deployment, host) = with_source(source.clone(), Some(allowed.clone()));
        let effective = resolve_effective(&deployment, &host)
            .unwrap_or_else(|error| panic!("{source} must resolve: {error}"));
        assert_eq!(
            effective.model.resolved_path.as_deref(),
            Some(expected.as_str())
        );
        assert!(effective.model.source.is_remote());
        // The frozen revision round-trips with the host's policy.
        let text = serde_json::to_string(&effective).unwrap();
        assert_eq!(
            capyctl_config::effective::decode_effective_snapshot(&text).unwrap(),
            effective,
            "{source}"
        );
    }

    // Unpinned or unsafe declarations are refused even where allowed.
    for source in [
        serde_json::json!({"type": "huggingface", "repo": "Qwen/Qwen3-4B"}),
        serde_json::json!({"type": "huggingface", "repo": "Qwen/Qwen3-4B", "revision": "main"}),
        serde_json::json!({"type": "huggingface", "repo": "", "revision": sha}),
        serde_json::json!({"type": "huggingface", "repo": "r", "revision": sha,
            "locked_commit": sha}),
        serde_json::json!({"type": "huggingface", "repo": "r", "revision": sha,
            "token_ref": "hf_plaintext"}),
        serde_json::json!({"type": "http", "url": "http://example.test/w.tar", "sha256": digest}),
        serde_json::json!({"type": "http", "url": "https://example.test/w.tar", "sha256": "abc"}),
        serde_json::json!({
            "type": "http", "url": "https://example.test/w.tar",
            "sha256": "z".repeat(64)
        }),
        serde_json::json!({"type": "local", "path": ""}),
        serde_json::json!({"type": "s3", "path": "/w"}),
    ] {
        let (deployment, host) = with_source(source.clone(), Some(allowed.clone()));
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
// T14
#[test]
fn requested_kv_above_ready_allocation_is_refused() {
    // The lab fixture's Ready phase allocates 8GiB, which is the memory request.
    let (mut deployment, host) = fixture();
    deployment["engine_config"]["memory"]["kv_cache"] = "8GiB".into();
    resolve_effective(&deployment, &host).expect("a request equal to the allocation is accepted");

    deployment["engine_config"]["memory"]["kv_cache"] = "9GiB".into();
    let error = resolve_effective(&deployment, &host).expect_err("9GiB exceeds the 8GiB Ready");
    assert_eq!(error.path, "engine_config.memory.kv_cache", "{error:?}");

    // The same bound applies to SGLang.
    let (mut deployment, mut host) = fixture();
    sglang_profile(&mut host);
    deployment["engine_config"]["memory"]["kv_cache"] = "9GiB".into();
    assert!(resolve_effective(&deployment, &host).is_err(), "sglang");
}

/// Spec §7: `model: { path }` predates `source` and keeps working, meaning exactly
/// a local source. Stating both is refused rather than resolved by precedence,
/// because a file that says two different things about its weights is a mistake.
// T14
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

/// The host's published device inventory digest (`runtime/sglang_device`
/// `capyctl-nvidia-inventory-v1`) is optional host policy: present, it must be the
/// exact lowercase hex digest the collector computes; absent, the native
/// launch carries placement as unasserted and fails closed.
#[test]
fn a_host_may_publish_a_device_inventory_digest() {
    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let (deployment, mut host) = fixture();
    host["device_inventory_digest"] = serde_json::json!(DIGEST);
    let effective = resolve_effective(&deployment, &host).unwrap();
    assert_eq!(
        effective.host.device_inventory_digest.as_deref(),
        Some(DIGEST)
    );
    // The digest survives the stored snapshot round-trip, which is the only
    // path the armed launch's frozen work reads.
    let snapshot = serde_json::to_value(&effective).unwrap();
    assert_eq!(snapshot["host"]["device_inventory_digest"], DIGEST);
    assert_eq!(
        capyctl_config::effective::decode_effective_snapshot(&snapshot.to_string())
            .unwrap()
            .host
            .device_inventory_digest
            .as_deref(),
        Some(DIGEST)
    );

    for bad in [
        "0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef",
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcde",
        "z123456789abcdef0123456789abcdef0123456789abcdef0123456789abcde",
    ] {
        let (deployment, mut host) = fixture();
        host["device_inventory_digest"] = serde_json::json!(bad);
        let error = resolve_effective(&deployment, &host).unwrap_err();
        assert!(
            error.to_string().contains("device_inventory_digest"),
            "{error}"
        );
    }

    // The strict document walk (management import path) allowlists the field
    // for the host kind, so a published digest survives the same gate every
    // other host field passes.
    let (deployment, mut host) = fixture();
    host["device_inventory_digest"] = serde_json::json!(DIGEST);
    let parsed = parse_strict(ConfigKind::Host, &serde_json::to_string(&host).unwrap()).unwrap();
    let effective = resolve_effective(&deployment, &parsed).unwrap();
    assert_eq!(
        effective.host.device_inventory_digest.as_deref(),
        Some(DIGEST)
    );
}

/// ADR 0008 / SPEC §15.3: the strict schema accepts both source spellings and
/// the host's `model_sources` block, and points a retired `locked_commit` at
/// the pinned `revision` instead of calling it unknown.
// T14
#[test]
fn strict_schema_accepts_model_sources_and_points_locked_commit_at_revision() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let deployment = |source: &str| {
        format!(
            "schema_version: 1\nkind: deployment\nname: d\nmodel:\n  source:\n{source}\n  content_fingerprint: sha256:x\n  revision: r1\n"
        )
    };
    for source in [
        format!("    huggingface:\n      repo: Qwen/Qwen3-4B\n      revision: {sha}\n      files: ['*.json']\n      token_ref: secret://hf"),
        format!("    type: huggingface\n    repo: Qwen/Qwen3-4B\n    revision: {sha}"),
        format!("    http:\n      url: https://example.test/w.tar\n      sha256: {}\n      archive: tar", "a".repeat(64)),
        "    local:\n      path: toy".to_string(),
    ] {
        parse_strict(ConfigKind::Deployment, &deployment(&source))
            .unwrap_or_else(|error| panic!("{source}: {error}"));
    }
    let error = parse_strict(
        ConfigKind::Deployment,
        &deployment(&format!(
            "    huggingface:\n      repo: r\n      revision: {sha}\n      locked_commit: {sha}"
        )),
    )
    .unwrap_err();
    assert!(error.detail.contains("revision"), "{error:?}");
    let host = "schema_version: 1\nkind: host\nname: h\nmodel_store:\n  path: /srv/models\nmodel_sources:\n  huggingface: allowed\n  http: denied\n  max_bytes: 200GiB\n  allowed_hosts: [huggingface.co]\n";
    parse_strict(ConfigKind::Host, host).unwrap();
    let unknown = format!("{host}  mirror: x\n");
    assert_eq!(
        parse_strict(ConfigKind::Host, &unknown).unwrap_err().code,
        ConfigErrorCode::UnknownField
    );
}

/// The example host document on its own, for host-policy rules (ADR 0019).
fn host() -> serde_json::Value {
    fixture().1
}

/// A discrete host: a system domain for host RAM and a device domain for the
/// GPU's own memory (ADR 0019, discrete GPU design §2).
fn discrete_host() -> serde_json::Value {
    let mut h = host();
    h["resource_policy"]["domains"] = serde_json::json!({
        "system": {"memory": "distinct", "managed_limit": "24GiB", "free_reserve": "8GiB",
                   "parked_limit": "16GiB", "host_kv_limit": "4GiB"},
        "gpu0": {"memory": "device", "device": "gpu0", "managed_limit": "14848MiB",
                 "free_reserve": "1536MiB", "parked_limit": "2GiB"}
    });
    h["resource_policy"]["devices"] =
        serde_json::json!({"gpu0": {"domain": "gpu0", "sharing": "shared"}});
    h
}

// T26: a discrete host declares a system domain and a device domain.
#[test]
fn a_discrete_host_policy_resolves_with_a_device_domain() {
    let policy = normalize_host_policy(&discrete_host()).expect("valid");
    let gpu = &policy.domains["gpu0"];
    assert_eq!(gpu.memory, DomainMemory::Device);
    assert_eq!(gpu.device.as_deref(), Some("gpu0"));
    assert_eq!(policy.domains["system"].device, None);
}

// T26: an existing unified host keeps resolving exactly as before.
#[test]
fn a_unified_host_policy_has_no_device_domain() {
    let policy = normalize_host_policy(&host()).expect("valid");
    assert_eq!(policy.domains["unified"].memory, DomainMemory::Unified);
    assert_eq!(policy.domains["unified"].device, None);
}

// T26: every broken device shape is refused with its path and its code.
#[test]
fn broken_device_domains_are_refused() {
    type Mutation = Box<dyn Fn(&mut serde_json::Value)>;
    let cases: Vec<(&str, &str, Mutation)> = vec![
        (
            "device domain without device",
            "device_policy_mismatch:",
            Box::new(|h| {
                h["resource_policy"]["domains"]["gpu0"]
                    .as_object_mut()
                    .unwrap()
                    .remove("device");
            }),
        ),
        (
            "unknown device",
            "device_policy_mismatch:",
            Box::new(|h| h["resource_policy"]["domains"]["gpu0"]["device"] = "gpu9".into()),
        ),
        (
            "device maps elsewhere",
            "device_policy_mismatch:",
            Box::new(|h| h["resource_policy"]["devices"]["gpu0"]["domain"] = "system".into()),
        ),
        (
            "another device maps to the device domain",
            "device_policy_mismatch:",
            Box::new(|h| {
                h["resource_policy"]["devices"]["gpu1"] =
                    serde_json::json!({"domain": "gpu0", "sharing": "shared"})
            }),
        ),
        (
            "host kv on device",
            "device_policy_mismatch:",
            Box::new(|h| h["resource_policy"]["domains"]["gpu0"]["host_kv_limit"] = "1GiB".into()),
        ),
        (
            "device on system domain",
            "device_policy_mismatch:",
            Box::new(|h| h["resource_policy"]["domains"]["system"]["device"] = "gpu0".into()),
        ),
        (
            "unified mixed with device",
            "unsupported_gpu_topology:",
            Box::new(|h| h["resource_policy"]["domains"]["system"]["memory"] = "unified".into()),
        ),
    ];
    for (name, prefix, mutate) in cases {
        let mut h = discrete_host();
        mutate(&mut h);
        let error = normalize_host_policy(&h).expect_err(name);
        assert_eq!(
            error.code,
            ConfigErrorCode::UnsupportedCombination,
            "{name}"
        );
        assert!(error.path.starts_with("resource_policy"), "{name}: {error}");
        assert!(error.detail.starts_with(prefix), "{name}: {error}");
    }
}

// T26: at most one unified domain per host.
#[test]
fn two_unified_domains_are_refused() {
    let mut h = host();
    let unified = h["resource_policy"]["domains"]["unified"].clone();
    h["resource_policy"]["domains"]["unified1"] = unified;
    let error = normalize_host_policy(&h).expect_err("two unified domains");
    assert_eq!(error.code, ConfigErrorCode::UnsupportedCombination);
    assert!(
        error.detail.starts_with("unsupported_gpu_topology:"),
        "{error}"
    );
}

// T26: two GPUs, each its own device domain (owner decision 3).
#[test]
fn two_device_domains_resolve() {
    let mut h = discrete_host();
    h["resource_policy"]["domains"]["gpu1"] = serde_json::json!({"memory": "device", "device": "gpu1",
        "managed_limit": "22GiB", "free_reserve": "2GiB", "parked_limit": "2GiB"});
    h["resource_policy"]["devices"]["gpu1"] =
        serde_json::json!({"domain": "gpu1", "sharing": "shared"});
    assert_eq!(normalize_host_policy(&h).unwrap().domains.len(), 3);
}

// T26: a device domain round-trips through the one writer of the host shape.
#[test]
fn a_device_domain_round_trips_through_composition() {
    use capyctl_config::effective::compose_resource_policy;
    use capyctl_config::resource_controls::ResourceContext;
    let policy = normalize_host_policy(&discrete_host()).unwrap();
    let composed = compose_resource_policy(
        &ResourceControls::from_host(&policy),
        &ResourceContext::from_host(&policy),
    );
    assert_eq!(composed["domains"]["gpu0"]["device"], "gpu0");
    assert_eq!(composed["domains"]["gpu0"]["memory"], "device");
    assert!(composed["domains"]["system"].get("device").is_none());
}

// Discrete GPU design §3: derived budgets for the three tiers on a discrete host.

const GIB: i64 = 1 << 30;

/// The fixture deployment with its budget derived from `engine_config.memory.request`
/// rather than declared, on the runtime profile named after `engine`.
fn deployment_with(residency: &str, engine: &str, request: &str) -> serde_json::Value {
    let (mut d, _) = fixture();
    d.as_object_mut().unwrap().remove("resources");
    d["residency"] = residency.into();
    d["runtime_profile"] = engine.into();
    d["engine_config"] = serde_json::json!({"memory": {"request": request, "kv_cache": "1GiB"}});
    d
}

/// The fixture deployment with explicit resources that name only `domain`.
fn deployment_with_resources(domain: &str) -> serde_json::Value {
    let (mut d, _) = fixture();
    d["runtime_profile"] = "vllm".into();
    let resources = d["resources"].as_object_mut().unwrap();
    for phase in resources.values_mut() {
        for allocation in phase["allocations"].as_array_mut().unwrap() {
            allocation["domain"] = domain.into();
            allocation["host_kv_bytes"] = "0B".into();
        }
    }
    d
}

/// The fixture deployment with explicit resources on a discrete host: the card
/// holds 12 GiB when Ready (room for the 8 GiB checkpoint and the 4 GiB KV
/// cache, review decision: the request is the device allocation), and host
/// RAM holds `system` in every phase.
fn deployment_with_discrete_resources(system: &str) -> serde_json::Value {
    let mut d = deployment_with_resources("gpu0");
    for (phase, device) in [
        ("cold", "14GiB"),
        ("ready", "12GiB"),
        ("parking", "12GiB"),
        ("parked", "2GiB"),
        ("wake", "14GiB"),
    ] {
        d["resources"][phase]["allocations"] = serde_json::json!([
            {"domain": "gpu0", "bytes": device, "host_kv_bytes": "0B"},
            {"domain": "system", "bytes": system, "host_kv_bytes": "0B"}
        ]);
    }
    d
}

/// A discrete host with two GPUs, each its own device domain (owner decision 3).
fn two_gpu_host() -> serde_json::Value {
    let mut h = discrete_host();
    h["resource_policy"]["domains"]["gpu1"] = serde_json::json!({"memory": "device",
        "device": "gpu1", "managed_limit": "14848MiB", "free_reserve": "1536MiB",
        "parked_limit": "2GiB"});
    h["resource_policy"]["devices"]["gpu1"] =
        serde_json::json!({"domain": "gpu1", "sharing": "shared"});
    h
}

/// Resolve against `host` with a `vllm` and an `sglang` profile and a checkpoint
/// whose weights are 8 GiB.
fn resolve(
    deployment: &serde_json::Value,
    host: &serde_json::Value,
) -> Result<capyctl_config::effective::EffectiveDeployment, capyctl_config::ConfigError> {
    resolve_weighing(deployment, host, Some(8 * GIB))
}

/// [`resolve`] with the checkpoint's weights stated (`None`: not yet measured).
fn resolve_weighing(
    deployment: &serde_json::Value,
    host: &serde_json::Value,
    weights_bytes: Option<i64>,
) -> Result<capyctl_config::effective::EffectiveDeployment, capyctl_config::ConfigError> {
    let mut host = host.clone();
    let mut sglang = host.clone();
    sglang_profile(&mut sglang);
    host["runtime_profiles"]["vllm"] = host["runtime_profiles"]["local"].clone();
    host["runtime_profiles"]["sglang"] = sglang["runtime_profiles"]["local"].clone();
    resolve_effective_with_checkpoint(
        deployment,
        &host,
        CheckpointFacts {
            weights_bytes,
            ..CheckpointFacts::default()
        },
    )
}

fn phase(p: &PhaseFootprint) -> Vec<(String, i64)> {
    p.allocations
        .iter()
        .map(|a| (a.domain.clone(), a.bytes))
        .collect()
}

// T26/T23: deep on a discrete host: device and system per phase.
#[test]
fn deep_budgets_charge_device_and_system() {
    let d = deployment_with("deep", "vllm", "10GiB");
    let r = resolve(&d, &discrete_host()).unwrap().resources;
    // The card holds the request and the engine's CUDA context and graphs.
    assert_eq!(
        phase(&r.ready),
        vec![
            (
                "gpu0".into(),
                10 * GIB + capyctl_config::effective::ENGINE_DEVICE_OVERHEAD_PLACEHOLDER_BYTES
            ),
            ("system".into(), ENGINE_HOST_OVERHEAD_PLACEHOLDER_BYTES)
        ]
    );
    assert_eq!(
        phase(&r.parked),
        vec![
            ("gpu0".into(), PARKED_DEVICE_RESIDUE_PLACEHOLDER_BYTES),
            ("system".into(), ENGINE_HOST_OVERHEAD_PLACEHOLDER_BYTES)
        ]
    );
    assert!(r.parked.devices.is_empty());
    assert_eq!(r.ready.devices.len(), 1);
}

// T26/T23: host_backed charges the pinned weights copy (1.5 times the
// weights) in every phase, for vLLM as for SGLang. Found live on a 16 GB
// discrete GPU: vLLM 0.29's pinned backup took 1.37 times the weights and
// stayed allocated after the wake.
#[test]
fn host_backed_charges_the_pinned_copy_in_every_phase() {
    for engine in ["vllm", "sglang"] {
        let r = resolve(
            &deployment_with("host_backed", engine, "10GiB"),
            &discrete_host(),
        )
        .unwrap()
        .resources;
        for p in [&r.cold, &r.ready, &r.parking, &r.parked, &r.wake] {
            assert_eq!(
                phase(p)[1].1,
                ENGINE_HOST_OVERHEAD_PLACEHOLDER_BYTES + 12 * GIB,
                "{engine}"
            );
        }
        assert_eq!(
            phase(&r.parked)[0].1,
            PARKED_DEVICE_RESIDUE_PLACEHOLDER_BYTES
        );
        assert!(r.parked.allocations.iter().all(|a| a.host_kv_bytes == 0));
    }
}

// T26: restart_only parks nothing; unified unchanged.
#[test]
fn restart_only_and_unified_are_unchanged() {
    let r = resolve(
        &deployment_with("restart_only", "vllm", "10GiB"),
        &discrete_host(),
    )
    .unwrap()
    .resources;
    assert!(r.parked.allocations.iter().all(|a| a.bytes == 0));
    let u = resolve(&deployment_with("deep", "vllm", "10GiB"), &host())
        .unwrap()
        .resources;
    assert_eq!(u.ready.allocations.len(), 1);
    assert_eq!(
        u.parked.allocations[0].bytes,
        PARKED_RESIDUAL_PLACEHOLDER_BYTES
    );
}

// T26: every discrete-host resolution refusal carries its code.
#[test]
fn discrete_refusals_are_typed() {
    let err = |d: &serde_json::Value, h: &serde_json::Value| {
        let e = resolve(d, h).unwrap_err();
        assert_eq!(e.code, ConfigErrorCode::UnsupportedCombination, "{e}");
        e.to_string()
    };
    assert!(err(&deployment_with_resources("gpu0"), &discrete_host())
        .contains("missing_system_allocation"));
    let mut two = deployment_with("deep", "vllm", "10GiB");
    two["devices"] = serde_json::json!([{"id": "gpu0", "sharing": "shared"}, {"id": "gpu1", "sharing": "shared"}]);
    assert!(err(&two, &two_gpu_host()).contains("multi_gpu_unsupported"));
    assert!(
        err(&deployment_with("host_backed", "vllm", "10GiB"), &host())
            .contains("host_backed_unavailable")
    );
}

// T26: explicit resources naming both domains resolve on a discrete host.
#[test]
fn explicit_resources_naming_both_domains_resolve() {
    let d = deployment_with_discrete_resources("4GiB");
    resolve(&d, &discrete_host()).expect("both domains named");
}

// T26 (review decision): explicit resources on a discrete host size the engine
// from the device allocation alone. The memory request is what the engine may
// use on the card (vLLM's utilization, SGLang's static fraction); the system
// allocation beside it is host RAM the engine process holds, and adding it
// would ask the card for memory it does not have.
#[test]
fn explicit_discrete_resources_size_the_engine_from_the_device_allocation() {
    let mut d = deployment_with_discrete_resources("4GiB");
    let resolved = resolve(&d, &discrete_host()).expect("both domains named");
    assert_eq!(resolved.engine_config.memory().request_bytes, 12 << 30);
    assert_eq!(
        resolved.ready_device_allocation(),
        Some((Some(0), 12 << 30))
    );
    // A declared request must match the device allocation, not the sum.
    d["engine_config"]["memory"]["request"] = "12GiB".into();
    resolve(&d, &discrete_host()).expect("the device allocation");
    d["engine_config"]["memory"]["request"] = "16GiB".into();
    assert!(resolve(&d, &discrete_host()).is_err());
    // A unified host keeps the whole Ready total.
    let (unified, host) = fixture();
    assert_eq!(
        resolve_effective(&unified, &host)
            .unwrap()
            .engine_config
            .memory()
            .request_bytes,
        8 << 30
    );
}

// T26 (review decision): a deployment that states only its KV cache on a
// discrete host (a Hugging Face or HTTP source, whose weights are known only
// once downloaded) is not materializable until the checkpoint digest measures
// the weights, which is what lets acceptance freeze it provisional (ADR 0014
// §7). Once measured, it is sized as the standalone template sizes a local
// checkpoint (design §3): weights x 1.10 plus the KV cache, at least 0.75 of the
// card for vLLM, with the startup peak on the card equal to the request. A
// request the device domain can never hold is refused with its code.
#[test]
fn a_discrete_request_derived_from_the_weights_is_sized_for_the_card() {
    let kv_only = |engine: &str| {
        let mut d = deployment_with("deep", engine, "1GiB");
        d["engine_config"] = serde_json::json!({"memory": {"kv_cache": "1GiB"}});
        d
    };
    for engine in ["vllm", "sglang"] {
        let e = resolve_weighing(&kv_only(engine), &discrete_host(), None).unwrap_err();
        assert_eq!(e.code, ConfigErrorCode::NotMaterializable, "{e}");
        assert!(e.path.starts_with("engine_config.memory"), "{e}");
        // Acceptance's placeholder: zero weights resolve to the KV cache
        // (vLLM: its floor), a bound nothing reserves until re-resolved.
        resolve_weighing(&kv_only(engine), &discrete_host(), Some(0)).expect("placeholder");
    }
    // SGLang: 8 GiB of weights x 1.10 plus 1 GiB of KV.
    let sglang = resolve(&kv_only("sglang"), &discrete_host()).unwrap();
    let request = 8 * GIB / 100 * 110 + GIB;
    assert_eq!(sglang.engine_config.memory().request_bytes, request);
    assert_eq!(sglang.engine_config.memory().startup_bytes, Some(request));
    let on_card = request + capyctl_config::effective::ENGINE_DEVICE_OVERHEAD_PLACEHOLDER_BYTES;
    assert_eq!(sglang.ready_device_allocation(), Some((Some(0), on_card)));
    assert_eq!(phase(&sglang.resources.cold)[0], ("gpu0".into(), on_card));
    // vLLM: at least 0.75 of the card the device domain declares (managed
    // limit plus free reserve, 16 GiB here).
    let vllm = resolve(&kv_only("vllm"), &discrete_host()).unwrap();
    let floor = 16 * GIB / 100 * 75;
    assert_eq!(vllm.engine_config.memory().request_bytes, floor);
    assert_eq!(vllm.engine_config.memory().startup_bytes, Some(floor));
    // 14 GiB of weights: 16.4 GiB, beyond the 14.5 GiB the card's domain manages.
    let e = resolve_weighing(&kv_only("sglang"), &discrete_host(), Some(14 * GIB)).unwrap_err();
    assert!(e.detail.starts_with("insufficient_device_memory:"), "{e}");
    // A unified host keeps the placeholder margin.
    let unified = resolve(&kv_only("sglang"), &host()).unwrap();
    assert_eq!(
        unified.engine_config.memory().request_bytes,
        8 * GIB + GIB + capyctl_config::effective::overhead_margin(Engine::Sglang)
    );
}

// T26: a discrete host whose policy has no single distinct system domain cannot
// derive the host overhead, so derivation refuses rather than omit it.
#[test]
fn derivation_needs_one_system_domain() {
    let mut h = discrete_host();
    h["resource_policy"]["domains"]["system2"] = h["resource_policy"]["domains"]["system"].clone();
    let e = resolve(&deployment_with("deep", "vllm", "10GiB"), &h).unwrap_err();
    assert!(e.detail.starts_with("missing_system_allocation:"), "{e}");
}

// T26: the host-RAM tier on a discrete host needs the weight size to charge the copy.
#[test]
fn host_backed_with_unknown_weights_is_not_materializable() {
    let mut host = discrete_host();
    host["runtime_profiles"]["vllm"] = host["runtime_profiles"]["local"].clone();
    let e = resolve_effective(&deployment_with("host_backed", "vllm", "10GiB"), &host).unwrap_err();
    // ADR 0014 §7: not materializable until the digest measures the weights,
    // which is what lets acceptance freeze the revision provisional.
    assert_eq!(
        e.code,
        capyctl_config::ConfigErrorCode::NotMaterializable,
        "{e}"
    );
    assert!(e.path.starts_with("engine_config.memory"), "{e}");
}

// T26: the host-RAM tier on a discrete host is allowed only when the system domain
// has room for the weights copy: its parked phase must fit the system domain's
// parked_limit and managed_limit.
#[test]
fn host_backed_is_refused_when_the_system_domain_has_no_room_for_the_copy() {
    let mut h = discrete_host();
    h["resource_policy"]["domains"]["system"]["parked_limit"] = "11GiB".into();
    let e = resolve(&deployment_with("host_backed", "vllm", "10GiB"), &h).unwrap_err();
    assert!(e.detail.starts_with("host_backed_unavailable:"), "{e}");
    // Deep parks no copy, so the same host takes it.
    resolve(&deployment_with("deep", "vllm", "10GiB"), &h).expect("deep keeps no copy");
    // Explicit resources are held to the same rule.
    let mut d = deployment_with_discrete_resources("12GiB");
    d["residency"] = "host_backed".into();
    let e = resolve(&d, &h).unwrap_err();
    assert!(e.detail.starts_with("host_backed_unavailable:"), "{e}");
}

// T26: one GPU per deployment on a discrete host, even when both devices would
// share a domain view.
#[test]
fn two_device_claims_are_refused_before_derivation() {
    let mut two = deployment_with_resources("gpu0");
    two["devices"] = serde_json::json!([{"id": "gpu0", "sharing": "shared"}, {"id": "gpu1", "sharing": "shared"}]);
    let e = resolve(&two, &two_gpu_host()).unwrap_err();
    assert!(e.detail.starts_with("multi_gpu_unsupported:"), "{e}");
}

// T26 (final review I9, ADR 0019 pin form): `devices: [{id: gpu0}]` pins the
// GPU and takes the sharing the host states for it; before, the claim failed
// to parse without `sharing`.
#[test]
fn the_short_pin_form_takes_the_hosts_sharing() {
    let mut d = deployment_with("deep", "vllm", "10GiB");
    d["devices"] = serde_json::json!([{"id": "gpu0"}]);
    let r = resolve(&d, &discrete_host()).expect("the short pin form resolves");
    assert_eq!(r.selected_devices.len(), 1);
    assert_eq!(r.selected_devices[0].id, "gpu0");
    assert_eq!(
        r.selected_devices[0].sharing,
        capyctl_config::effective::Sharing::Shared
    );
    // A stated sharing is kept.
    d["devices"] = serde_json::json!([{"id": "gpu0", "sharing": "exclusive"}]);
    let r = resolve(&d, &discrete_host()).unwrap();
    assert_eq!(
        r.selected_devices[0].sharing,
        capyctl_config::effective::Sharing::Exclusive
    );
}

// T26 (re-review parity rule): the engine's CUDA context and graphs are
// charged beside the request by one rule on a unified pool and on a card, and
// the revision records the charge so a snapshot re-derives it; a revision
// frozen before the charge (no `overhead_bytes`) still decodes without it.
#[test]
fn the_cuda_context_is_charged_alike_on_unified_and_discrete_hosts() {
    let overhead = capyctl_config::effective::ENGINE_DEVICE_OVERHEAD_PLACEHOLDER_BYTES;
    let d = deployment_with("deep", "vllm", "10GiB");
    for host in [host(), discrete_host()] {
        let r = resolve(&d, &host).unwrap();
        assert_eq!(r.resources.ready.allocations[0].bytes, 10 * GIB + overhead);
        assert_eq!(r.engine_config.memory().overhead_bytes, Some(overhead));
        let snapshot = serde_json::to_string(&r).unwrap();
        assert_eq!(
            capyctl_config::effective::decode_effective_snapshot(&snapshot).unwrap(),
            r
        );
    }
}

// T26 T27, SPEC §7.3: a declared recipe whose Ready phase claims a device its
// cold phase does not is refused. The start's completion would otherwise write
// that exclusive claim into the ledger without the device-conflict check.
#[test]
fn a_ready_phase_may_not_claim_a_device_its_cold_phase_lacks() {
    let (mut deployment, host) = fixture();
    let exclusive = serde_json::json!([{"id": "gpu0", "sharing": "exclusive"}]);
    deployment["devices"] = exclusive.clone();
    for phase in ["cold", "ready", "parking", "wake"] {
        deployment["resources"][phase]["devices"] = exclusive.clone();
    }
    resolve_effective(&deployment, &host).expect("matching claims resolve");
    deployment["resources"]["cold"]["devices"] = serde_json::json!([]);
    let error = resolve_effective(&deployment, &host).expect_err("must be refused");
    assert!(error.to_string().contains("resources"), "{error}");
}
