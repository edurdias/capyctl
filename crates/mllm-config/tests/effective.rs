use mllm_config::effective::{
    derive_default_managed_ceiling, parse_bytes, parse_duration_ms, resolve_effective, Engine,
};
use mllm_config::{parse_strict, ConfigErrorCode, ConfigKind};

fn fixture() -> (serde_json::Value, serde_json::Value) {
    let all: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/f2-deployment.json")).expect("fixture JSON");
    (all["deployment"].clone(), all["host"].clone())
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
    assert_eq!(effective.qualification_fingerprint.len(), 64);
    assert!(!serde_json::to_string(&effective)
        .unwrap()
        .contains("secret-value"));
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
    let unknown = "schema_version: 1\nkind: deployment\nname: d\nmodel:\n  path: /m\n  content_fingerprint: fp\n  revision: r\nroutes: [r]\nruntime_profile: p\nruntime_profile_revision: 1\nrecipe: r\nresidency: warm\nrecovery: reconcile\ndevices: []\nresources:\n  ready:\n    surprise: true\n";
    let error = parse_strict(ConfigKind::Deployment, unknown).unwrap_err();
    assert_eq!(error.code, ConfigErrorCode::UnknownField);

    let wrong = "schema_version: 1\nkind: deployment\nname: d\nmodel:\n  path: /m\n  content_fingerprint: fp\n  revision: r\nroutes: route-a\nruntime_profile: p\nruntime_profile_revision: 1\nrecipe: r\nresidency: warm\nrecovery: reconcile\ndevices: []\nresources: {}\n";
    assert!(parse_strict(ConfigKind::Deployment, wrong).is_err());
}

#[test]
fn strict_yaml_rejects_duplicate_nested_keys() {
    let yaml = include_str!("fixtures/f2-deployment.yaml");
    let error = parse_strict(ConfigKind::Deployment, yaml).unwrap_err();
    assert_eq!(error.code, ConfigErrorCode::DuplicateKey);
}
