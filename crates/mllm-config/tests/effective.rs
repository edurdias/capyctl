use mllm_config::effective::{
    binding_fingerprint, derive_default_managed_ceiling, parse_bytes, parse_duration_ms,
    resolve_effective, DomainMemory, Engine,
};
use mllm_config::resource_controls::ResourceControls;
use mllm_config::{parse_strict, ConfigErrorCode, ConfigKind};
use mllm_domain::launch::ProfileLaunchSettings;

fn fixture() -> (serde_json::Value, serde_json::Value) {
    let all: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/f2-deployment.json")).expect("fixture JSON");
    (all["deployment"].clone(), all["host"].clone())
}

/// Set the topology of the lab host's only domain. Used by this task and Task 4.
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

#[test]
fn ordinary_engine_compatibility_goldens() {
    for engine in ["vllm", "sglang", "fake"] {
        let (deployment, mut host) = fixture();
        let profile = &mut host["runtime_profiles"]["local"];
        if engine != "vllm" {
            profile["engine"] = engine.into();
            profile["args"] = serde_json::json!([]);
            profile["launch_settings"] = if engine == "sglang" {
                profile["security"]["admin_credential_ref"] = "secret://admin-key".into();
                serde_json::json!({"engine":"sglang", "recipe":"qwen3_4b_instruct2507_tp1_dp1_bf16_disk_reload_v1", "requested_budget":{"kv_cache_bytes":"4GiB", "static_memory_fraction_bps":7500}})
            } else {
                serde_json::json!({"engine":"fake"})
            };
        }
        let effective = resolve_effective(&deployment, &host).unwrap();
        let golden: serde_json::Value = serde_json::from_str(match engine {
            "vllm" => include_str!("fixtures/effective-vllm-golden.json"),
            "sglang" => include_str!("fixtures/effective-sglang-golden.json"),
            _ => include_str!("fixtures/effective-fake-golden.json"),
        })
        .unwrap();
        assert_eq!(
            serde_json::to_value(effective).unwrap(),
            golden["effective"],
            "{engine}"
        );
    }
}

fn qualification_policy() -> serde_json::Value {
    serde_json::json!({
        "revision": 1,
        "allow_qualification_runs": true,
        "allow_experimental_controls": false,
        "allowed_manifest_digests": [
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            "0000000000000000000000000000000000000000000000000000000000000000"
        ],
        "max_run_duration": "24h",
        "max_cleanup_duration": "1h",
        "max_cases": 128,
        "max_requests": 4096,
        "max_request_body_bytes": "1MiB",
        "max_input_tokens_per_request": 131072,
        "max_output_tokens_per_request": 16384
    })
}

#[test]
fn qualification_policy_is_optional_and_normalized_without_authorizing_runs() {
    let (deployment, host) = fixture();
    let absent = resolve_effective(&deployment, &host).unwrap();
    assert_eq!(absent.host.qualification_policy, None);

    let mut host = host;
    host["qualification_policy"] = qualification_policy();
    let effective = resolve_effective(&deployment, &host).unwrap();
    let policy = effective.host.qualification_policy.unwrap();
    assert_eq!(policy.revision, 1);
    assert!(policy.allow_qualification_runs);
    assert!(!policy.allow_experimental_controls);
    assert_eq!(policy.max_run_duration_ms, 86_400_000);
    assert_eq!(policy.max_cleanup_duration_ms, 3_600_000);
    assert_eq!(policy.max_request_body_bytes, 1 << 20);
    assert!(policy
        .allowed_manifest_digests
        .windows(2)
        .all(|w| w[0] < w[1]));
}

#[test]
fn qualification_permissions_are_independent_and_empty_allowlist_is_deny_all() {
    for permissions in [(false, false), (false, true), (true, false), (true, true)] {
        let (deployment, mut host) = fixture();
        let mut policy = qualification_policy();
        policy["allow_qualification_runs"] = permissions.0.into();
        policy["allow_experimental_controls"] = permissions.1.into();
        policy["allowed_manifest_digests"] = serde_json::json!([]);
        host["qualification_policy"] = policy;
        let normalized = resolve_effective(&deployment, &host)
            .unwrap()
            .host
            .qualification_policy
            .unwrap();
        assert_eq!(normalized.allow_qualification_runs, permissions.0);
        assert_eq!(normalized.allow_experimental_controls, permissions.1);
        assert!(normalized.allowed_manifest_digests.is_empty());
    }
}

#[test]
fn qualification_policy_bounds_and_digest_rules_fail_closed() {
    let digest = "0".repeat(64);
    let (deployment, mut host) = fixture();
    let mut exact_digest_limit = qualification_policy();
    exact_digest_limit["allowed_manifest_digests"] = serde_json::json!((0..1024)
        .map(|value| format!("{value:064x}"))
        .collect::<Vec<_>>());
    host["qualification_policy"] = exact_digest_limit;
    assert_eq!(
        resolve_effective(&deployment, &host)
            .unwrap()
            .host
            .qualification_policy
            .unwrap()
            .allowed_manifest_digests
            .len(),
        1024
    );
    for (field, value) in [
        ("revision", serde_json::json!(0)),
        ("max_run_duration", serde_json::json!("86400001ms")),
        ("max_cleanup_duration", serde_json::json!("3600001ms")),
        ("max_cases", serde_json::json!(129)),
        ("max_requests", serde_json::json!(4097)),
        ("max_request_body_bytes", serde_json::json!("1048577B")),
        ("max_input_tokens_per_request", serde_json::json!(131073)),
        ("max_output_tokens_per_request", serde_json::json!(16385)),
    ] {
        let (deployment, mut host) = fixture();
        let mut policy = qualification_policy();
        policy[field] = value;
        host["qualification_policy"] = policy;
        assert!(resolve_effective(&deployment, &host).is_err(), "{field}");
    }
    for bad in [
        "A".repeat(64),
        "0".repeat(63),
        format!("{}g", "0".repeat(63)),
    ] {
        let (deployment, mut host) = fixture();
        let mut policy = qualification_policy();
        policy["allowed_manifest_digests"] = serde_json::json!([bad]);
        host["qualification_policy"] = policy;
        assert!(resolve_effective(&deployment, &host).is_err());
    }
    for digests in [vec![digest.clone(), digest], vec!["0".repeat(64); 1025]] {
        let (deployment, mut host) = fixture();
        let mut policy = qualification_policy();
        policy["allowed_manifest_digests"] = serde_json::json!(digests);
        host["qualification_policy"] = policy;
        assert!(resolve_effective(&deployment, &host).is_err());
    }
}

#[test]
fn qualification_policy_requires_complete_typed_bounded_input() {
    for field in [
        "revision",
        "allow_qualification_runs",
        "allow_experimental_controls",
        "allowed_manifest_digests",
        "max_run_duration",
        "max_cleanup_duration",
        "max_cases",
        "max_requests",
        "max_request_body_bytes",
        "max_input_tokens_per_request",
        "max_output_tokens_per_request",
    ] {
        let (deployment, mut host) = fixture();
        let mut policy = qualification_policy();
        policy.as_object_mut().unwrap().remove(field);
        host["qualification_policy"] = policy;
        let error = resolve_effective(&deployment, &host).unwrap_err();
        assert_eq!(
            error.code,
            ConfigErrorCode::MissingRequired,
            "{field}: {error:?}"
        );
        assert!(error.path.ends_with(field), "{field}: {error:?}");
    }
    for (field, value) in [
        ("revision", serde_json::json!("secret-value")),
        ("max_cases", serde_json::json!(-1)),
        ("max_requests", serde_json::json!(18446744073709551615u64)),
    ] {
        let (deployment, mut host) = fixture();
        let mut policy = qualification_policy();
        policy[field] = value;
        host["qualification_policy"] = policy;
        let error = resolve_effective(&deployment, &host).unwrap_err();
        assert!(error.path.ends_with(field), "{error:?}");
        assert!(!error.to_string().contains("secret-value"));
    }
    let (deployment, mut host) = fixture();
    let mut policy = qualification_policy();
    policy["unknown"] = serde_json::json!("secret-value");
    host["qualification_policy"] = policy;
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert_eq!(error.code, ConfigErrorCode::UnknownField);
    assert!(
        error.path.ends_with("qualification_policy.unknown"),
        "{error:?}"
    );
    assert!(!error.to_string().contains("secret-value"));
}

#[test]
fn qualification_policy_rejects_zero_overflow_and_oversized_encoding() {
    for field in [
        "revision",
        "max_cases",
        "max_requests",
        "max_input_tokens_per_request",
        "max_output_tokens_per_request",
    ] {
        let (deployment, mut host) = fixture();
        let mut policy = qualification_policy();
        policy[field] = 0.into();
        host["qualification_policy"] = policy;
        assert!(resolve_effective(&deployment, &host).is_err(), "{field}");
    }
    for (field, value) in [
        ("max_run_duration", "0ms"),
        ("max_cleanup_duration", "0ms"),
        ("max_request_body_bytes", "0B"),
        ("max_run_duration", "9223372036854775808ms"),
        ("max_cleanup_duration", "9223372036854775808ms"),
        ("max_request_body_bytes", "9223372036854775808B"),
    ] {
        let (deployment, mut host) = fixture();
        let mut policy = qualification_policy();
        policy[field] = value.into();
        host["qualification_policy"] = policy;
        assert!(resolve_effective(&deployment, &host).is_err(), "{field}");
    }
    let (deployment, mut host) = fixture();
    let mut policy = qualification_policy();
    policy["max_run_duration"] = format!("{}ms", "1".repeat(1 << 20)).into();
    host["qualification_policy"] = policy;
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert!(error.to_string().contains("encoding exceeds 1MiB"));
}

#[test]
fn qualification_policy_is_not_part_of_recipe_qualification_identity() {
    let (deployment, host) = fixture();
    let original = resolve_effective(&deployment, &host).unwrap();
    let mut changed = host;
    changed["qualification_policy"] = qualification_policy();
    let changed = resolve_effective(&deployment, &changed).unwrap();
    assert_eq!(
        original.qualification_fingerprint,
        changed.qualification_fingerprint
    );
    assert_ne!(
        original.host.qualification_policy,
        changed.host.qualification_policy
    );
}

#[test]
fn normalized_policy_exposes_shared_fail_closed_validation() {
    let (deployment, mut host) = fixture();
    host["qualification_policy"] = qualification_policy();
    let policy = resolve_effective(&deployment, &host)
        .unwrap()
        .host
        .qualification_policy
        .unwrap();
    assert!(policy.validate().is_ok());

    let mut stale_or_forged = policy.clone();
    stale_or_forged.max_requests = 4097;
    assert!(stale_or_forged.validate().is_err());
    let mut noncanonical = policy;
    noncanonical.allowed_manifest_digests.reverse();
    assert!(noncanonical.validate().is_err());
}

#[test]
fn strict_yaml_accepts_only_complete_qualification_policy_shape() {
    let yaml = "schema_version: 1\nkind: host\nname: h\nqualification_policy:\n  revision: 1\n  allow_qualification_runs: false\n  allow_experimental_controls: true\n  allowed_manifest_digests: []\n  max_run_duration: 24h\n  max_cleanup_duration: 60m\n  max_cases: 128\n  max_requests: 4096\n  max_request_body_bytes: 1024KiB\n  max_input_tokens_per_request: 131072\n  max_output_tokens_per_request: 16384\n";
    assert!(parse_strict(ConfigKind::Host, yaml).is_ok());
    assert_eq!(
        parse_strict(ConfigKind::Host, &yaml.replace("  max_cases: 128\n", ""))
            .unwrap_err()
            .code,
        ConfigErrorCode::MissingRequired
    );
    assert_eq!(
        parse_strict(
            ConfigKind::Host,
            &yaml.replace("  max_cases: 128", "  surprise: 128")
        )
        .unwrap_err()
        .code,
        ConfigErrorCode::UnknownField
    );
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
        effective.profile.qualification_id,
        "qualification-evidence-17"
    );
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
    assert_eq!(effective.qualification_fingerprint.len(), 64);
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
        changed.qualification_fingerprint,
        original.qualification_fingerprint
    );
}

#[test]
fn owned_launch_setting_mutations_change_qualification_identity() {
    let (deployment, host) = fixture();
    let original = resolve_effective(&deployment, &host)
        .unwrap()
        .qualification_fingerprint;
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
        assert_ne!(original, changed.qualification_fingerprint, "{pointer}");
    }
    let mut restart = deployment.clone();
    restart["residency"] = "restart_only".into();
    let baseline = resolve_effective(&restart, &host)
        .unwrap()
        .qualification_fingerprint;
    let mut changed_host = host;
    changed_host["runtime_profiles"]["local"]["launch_settings"]["enable_sleep_mode"] =
        false.into();
    assert_ne!(
        baseline,
        resolve_effective(&restart, &changed_host)
            .unwrap()
            .qualification_fingerprint
    );
}

#[test]
fn qualification_dimensions_fail_to_alias() {
    let (deployment, host) = fixture();
    let original = resolve_effective(&deployment, &host)
        .unwrap()
        .qualification_fingerprint;
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
        assert_ne!(original, changed.qualification_fingerprint, "{pointer}");
    }

    let mut changed_deployment = deployment.clone();
    let mut changed_host = host.clone();
    changed_deployment["runtime_profile_revision"] = 8.into();
    changed_host["runtime_profiles"]["local"]["revision"] = 8.into();
    assert_ne!(
        original,
        resolve_effective(&changed_deployment, &changed_host)
            .unwrap()
            .qualification_fingerprint
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
            .qualification_fingerprint
    );

    let mut auth_structure = host.clone();
    auth_structure["runtime_profiles"]["local"]["security"]["admin_credential_ref"] =
        "secret://admin".into();
    assert_ne!(
        original,
        resolve_effective(&deployment, &auth_structure)
            .unwrap()
            .qualification_fingerprint
    );

    let mut restart = deployment.clone();
    restart["residency"] = "restart_only".into();
    let mut controls = host.clone();
    controls["runtime_profiles"]["local"]["launch_settings"]["enable_sleep_mode"] = false.into();
    let enabled = resolve_effective(&restart, &controls)
        .unwrap()
        .qualification_fingerprint;
    controls["runtime_profiles"]["local"]["security"]["experimental_controls"] = false.into();
    assert_ne!(
        enabled,
        resolve_effective(&restart, &controls)
            .unwrap()
            .qualification_fingerprint
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
    let (deployment, mut host) = fixture();
    host["runtime_profiles"]["local"]["launch_settings"]["engine"] = "fake".into();
    assert!(resolve_effective(&deployment, &host).is_err());
}

#[test]
fn equivalent_byte_units_have_identical_qualification_identity() {
    let (deployment, host) = fixture();
    let original = resolve_effective(&deployment, &host).unwrap();
    let mut equivalent = host;
    equivalent["runtime_profiles"]["local"]["launch_settings"]["requested_budget"]
        ["kv_cache_bytes"] = "4096MiB".into();
    let normalized = resolve_effective(&deployment, &equivalent).unwrap();
    assert_eq!(
        original.qualification_fingerprint,
        normalized.qualification_fingerprint
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
        original.qualification_fingerprint,
        normalized.qualification_fingerprint
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
fn binding_dimensions_change_binding_not_qualification_identity() {
    let (deployment, host) = fixture();
    let qualification = resolve_effective(&deployment, &host)
        .unwrap()
        .qualification_fingerprint;
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
            .qualification_fingerprint
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
        snapshot.qualification_fingerprint,
        later.qualification_fingerprint
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
fn credential_reference_values_do_not_change_qualification_identity() {
    let (deployment, host) = fixture();
    let original = resolve_effective(&deployment, &host).unwrap();
    let mut edited = host;
    edited["runtime_profiles"]["local"]["security"]["credential_ref"] = "secret://rotated".into();
    let rotated = resolve_effective(&deployment, &edited).unwrap();
    assert_eq!(
        original.qualification_fingerprint,
        rotated.qualification_fingerprint
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
        changed.qualification_fingerprint,
        original.qualification_fingerprint
    );
}

#[test]
fn engines_without_reviewed_argument_allowlists_accept_only_empty_args() {
    for engine in ["sglang", "fake"] {
        let (deployment, mut host) = fixture();
        host["runtime_profiles"]["local"]["engine"] = engine.into();
        host["runtime_profiles"]["local"]["args"] = serde_json::json!(["--max-model-len", "4096"]);
        host["runtime_profiles"]["local"]["launch_settings"] = if engine == "sglang" {
            serde_json::json!({"engine":"sglang", "recipe":"qwen3_4b_instruct2507_tp1_dp1_bf16_disk_reload_v1", "requested_budget":{"kv_cache_bytes":"4GiB", "static_memory_fraction_bps":7500}})
        } else {
            serde_json::json!({"engine":"fake"})
        };
        if engine == "sglang" {
            host["runtime_profiles"]["local"]["security"]["admin_credential_ref"] =
                "secret://admin".into();
        }
        assert!(resolve_effective(&deployment, &host).is_err(), "{engine}");
    }
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

    let wrong = "schema_version: 1\nkind: deployment\nname: d\nmodel:\n  path: /m\n  content_fingerprint: fp\n  revision: r\nroutes: route-a\nruntime_profile: p\nruntime_profile_revision: 1\nrecipe: r\nresidency: warm\nrecovery: reconcile\ndevices: []\nresources: {}\n";
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

#[test]
fn strict_yaml_rejects_duplicate_nested_keys() {
    let yaml = include_str!("fixtures/f2-deployment.yaml");
    let error = parse_strict(ConfigKind::Deployment, yaml).unwrap_err();
    assert_eq!(error.code, ConfigErrorCode::DuplicateKey);
}
