//! ADR 0023: the TensorFold engine kind, its option policy and its resolution.
//! CPU tests only; none of this qualifies TensorFold.

use capyctl_config::effective::{resolve_effective, TENSORFOLD_FIRST_BUILD_MS};
use capyctl_config::engine_policy::{
    sensitivity, validate_extra_args, Engine, ExtraArgsContext, Sensitivity,
};
use capyctl_config::registration::is_verified;
use capyctl_config::ConfigErrorCode;
use capyctl_domain::launch::LaunchSettings;
use serde_json::{json, Value};
use std::collections::BTreeSet;

fn fixture() -> (Value, Value) {
    let all: Value =
        serde_json::from_str(include_str!("fixtures/f2-deployment.json")).expect("fixture JSON");
    let (mut deployment, mut host) = (all["deployment"].clone(), all["host"].clone());
    let profile = &mut host["runtime_profiles"]["local"];
    profile["engine"] = "tensorfold".into();
    profile["executable"] = "/opt/tf/bin/tensorfold".into();
    profile["build_fingerprint"] = "0.6.0".into();
    profile["args"] = json!([]);
    profile["security"]["deep_park"] = "disabled".into();
    profile["security"]["approved_paths"] = json!(["/srv/drafters"]);
    deployment["residency"] = "restart_only".into();
    deployment["engine_config"] = json!({"context_length": 8192});
    (deployment, host)
}

fn context<'a>(
    approved: &'a BTreeSet<String>,
    fixed: &'a BTreeSet<String>,
) -> ExtraArgsContext<'a> {
    ExtraArgsContext {
        engine: Engine::Tensorfold,
        sleep_mode: false,
        approved_options: approved,
        approved_paths: &[],
        checkpoint_root: None,
        host_fixed: fixed,
    }
}

// T41 T01: the serde name, the closed name list and the verified set.
#[test]
fn tensorfold_is_a_named_engine_with_a_verified_version() {
    assert_eq!(Engine::from_name("tensorfold"), Some(Engine::Tensorfold));
    assert_eq!(Engine::Tensorfold.name(), "tensorfold");
    assert_eq!(
        serde_json::to_value(Engine::Tensorfold).unwrap(),
        "tensorfold"
    );
    assert_eq!(Engine::from_name("TensorFold"), None);
    assert!(is_verified(Engine::Tensorfold, "0.6.0"));
    assert!(!is_verified(Engine::Tensorfold, "0.6.1"));
}

// T41 T14: every reserved flag is refused, abbreviated and as a value form.
#[test]
fn reserved_tensorfold_flags_are_refused_in_every_spelling() {
    let none = BTreeSet::new();
    for args in [
        json!(["--host", "0.0.0.0"]),
        json!(["--port=9000"]),
        json!(["--name", "x"]),
        json!(["--alias", "y"]),
        json!(["--backend", "mlx"]),
        json!(["--context", "4096"]),
        json!(["--tp", "2"]),
        json!(["--rank", "1"]),
        json!(["--master", "10.0.0.1"]),
        json!(["--master-port", "29551"]),
        json!(["--snapshot-dir", "/tmp/s"]),
        json!(["--no-update-check"]),
        json!(["--snapshot", "/tmp/s"]),
        json!(["--conte", "4096"]),
    ] {
        let args: Vec<String> = serde_json::from_value(args.clone()).unwrap();
        let error = validate_extra_args(&args, &context(&none, &none))
            .expect_err(&format!("{args:?} must be refused"));
        assert!(error.to_string().contains("reserved"), "{args:?}: {error}");
    }
}

// T41 T14: a typed field's native spelling in extra_args is a duplicate.
#[test]
fn typed_tensorfold_spellings_are_refused_in_extra_args() {
    let none = BTreeSet::new();
    for (args, field) in [
        (vec!["--kv-dtype", "int8"], "kv_cache_dtype"),
        (vec!["--max-tokens", "512"], "tensorfold.max_tokens"),
        (vec!["--no-thinking"], "tensorfold.thinking"),
    ] {
        let args: Vec<String> = args.into_iter().map(str::to_owned).collect();
        let error = validate_extra_args(&args, &context(&none, &none)).unwrap_err();
        assert!(error.to_string().contains(field), "{args:?}: {error}");
    }
}

// T41 T37 (Review Focus 3): `--vision` is ordinary although it is a prefix of
// `--vision-urls`, which needs named approval; `--lane-kernels` loads code.
#[test]
fn vision_is_ordinary_and_vision_urls_needs_approval() {
    let none = BTreeSet::new();
    assert_eq!(sensitivity(Engine::Tensorfold, "--vision"), None);
    assert_eq!(
        sensitivity(Engine::Tensorfold, "--vision-urls"),
        Some(Sensitivity::ListenerOrEgress)
    );
    assert_eq!(
        sensitivity(Engine::Tensorfold, "--lane-kernels"),
        Some(Sensitivity::Code)
    );
    let ok: Vec<String> = vec!["--vision".into(), "--parallel".into(), "2".into()];
    validate_extra_args(&ok, &context(&none, &none)).unwrap();
    let egress: Vec<String> = vec!["--vision-urls".into()];
    assert!(validate_extra_args(&egress, &context(&none, &none)).is_err());
    let approved: BTreeSet<String> = ["--vision-urls".to_owned()].into();
    validate_extra_args(&egress, &context(&approved, &none)).unwrap();
}

// T41 T14: the typed block maps to settings; restart_only is the residency.
#[test]
fn a_tensorfold_deployment_resolves_restart_only_settings() {
    let (mut deployment, host) = fixture();
    deployment["engine_config"] = json!({
        "context_length": 8192, "kv_cache_dtype": "int8",
        "tensorfold": {"max_tokens": 1024, "thinking": false}
    });
    let effective = resolve_effective(&deployment, &host).unwrap();
    assert_eq!(effective.profile.engine, Engine::Tensorfold);
    assert_eq!(
        effective.residency,
        capyctl_config::effective::Residency::RestartOnly
    );
    let LaunchSettings::Tensorfold(settings) = &effective.engine_config else {
        panic!("TensorFold settings");
    };
    assert_eq!(settings.common.context_length, Some(8192));
    assert_eq!(settings.common.kv_cache_dtype.as_deref(), Some("int8"));
    assert_eq!(settings.max_tokens, Some(1024));
    assert_eq!(settings.thinking, Some(false));
}

// T41 T03: context_length and resources are required; deep is refused with
// capability_missing; foreign typed fields are refused with their path.
#[test]
fn tensorfold_requirements_are_refused_with_their_path() {
    let (mut deployment, host) = fixture();
    deployment["engine_config"] = json!({});
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert_eq!(error.path, "engine_config.context_length", "{error}");

    let (mut deployment, host) = fixture();
    deployment.as_object_mut().unwrap().remove("resources");
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert_eq!(error.code, ConfigErrorCode::MissingRequired, "{error}");
    assert_eq!(error.path, "resources", "{error}");

    for residency in ["deep", "host_backed"] {
        let (mut deployment, host) = fixture();
        deployment["residency"] = residency.into();
        let error = resolve_effective(&deployment, &host).unwrap_err();
        assert!(error.detail.starts_with("capability_missing"), "{error}");
    }
    // An operator who wrote deep_park: enabled on the profile still gets it.
    let (mut deployment, mut host) = fixture();
    host["runtime_profiles"]["local"]["security"]["deep_park"] = "enabled".into();
    deployment["residency"] = "deep".into();
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert!(error.detail.starts_with("capability_missing"), "{error}");

    for field in [
        "dtype",
        "quantization",
        "max_concurrent_requests",
        "cuda_graphs",
        "language_model_only",
        "trust_remote_code",
    ] {
        let (mut deployment, host) = fixture();
        let value = match field {
            "max_concurrent_requests" => json!(4),
            "cuda_graphs" | "language_model_only" | "trust_remote_code" => json!(true),
            "dtype" => json!("bfloat16"),
            _ => json!("fp8"),
        };
        deployment["engine_config"][field] = value;
        let error = resolve_effective(&deployment, &host).unwrap_err();
        assert_eq!(error.path, format!("engine_config.{field}"), "{error}");
    }
    let (mut deployment, host) = fixture();
    deployment["engine_config"]["vllm"] = json!({"block_size_tokens": 16});
    assert_eq!(
        resolve_effective(&deployment, &host).unwrap_err().path,
        "engine_config.vllm"
    );
}

// T41 T37 (Review Focus 2, ruling 7): `--drafter` is a path option, as
// SGLang's `--speculative-draft-model-path`: it needs named approval, and its
// value must lie inside the approved paths; a repository id is refused. The
// typed block has no drafter field.
#[test]
fn a_drafter_repository_id_is_refused() {
    let none = BTreeSet::new();
    let approved: BTreeSet<String> = ["--drafter".to_owned()].into();
    let paths = [std::path::PathBuf::from("/srv/drafters")];
    let with_paths = |approved| ExtraArgsContext {
        approved_paths: &paths,
        ..context(approved, &none)
    };
    assert_eq!(
        sensitivity(Engine::Tensorfold, "--drafter"),
        Some(Sensitivity::Path {
            checkpoint_exempt: false
        })
    );
    let args = |value: &str| vec!["--drafter".to_owned(), value.to_owned()];
    assert!(
        validate_extra_args(&args("/srv/drafters/d"), &with_paths(&none)).is_err(),
        "needs approval"
    );
    validate_extra_args(&args("/srv/drafters/d"), &with_paths(&approved)).unwrap();
    for refused in [
        "z-lab/Qwen3.8-27B-DFlash2",
        "drafters/d",
        "/elsewhere/d",
        "/srv/drafters/../etc",
    ] {
        assert!(
            validate_extra_args(&args(refused), &with_paths(&approved)).is_err(),
            "{refused}"
        );
    }
    let (mut deployment, host) = fixture();
    deployment["engine_config"]["tensorfold"] = json!({"drafter": "/srv/drafters/d"});
    assert!(
        resolve_effective(&deployment, &host).is_err(),
        "no typed drafter field"
    );
}

// T41 T14 (ruling 8): the first-build bound and the request deadline floor.
#[test]
fn an_undeclared_initialize_timeout_covers_the_first_build() {
    let (mut deployment, host) = fixture();
    deployment
        .as_object_mut()
        .unwrap()
        .remove("request_deadline");
    let effective = resolve_effective(&deployment, &host).unwrap();
    assert_eq!(effective.timeouts.initialize_ms, TENSORFOLD_FIRST_BUILD_MS);
    assert!(effective.request_deadline_ms >= TENSORFOLD_FIRST_BUILD_MS);
    let (mut deployment, host) = fixture();
    deployment["request_deadline"] = "600s".into();
    deployment["timeouts"] = json!({"initialize": "300s"});
    let effective = resolve_effective(&deployment, &host).unwrap();
    assert_eq!(effective.timeouts.initialize_ms, 300_000);
    assert_eq!(effective.request_deadline_ms, 600_000);
}

// T41 T08: a TensorFold revision snapshots and re-resolves exactly.
#[test]
fn a_tensorfold_snapshot_decodes_exactly() {
    let (mut deployment, host) = fixture();
    deployment["engine_config"]["tensorfold"] = json!({"max_tokens": 256, "thinking": true});
    let effective = resolve_effective(&deployment, &host).unwrap();
    let raw = serde_json::to_value(&effective).unwrap().to_string();
    assert_eq!(
        capyctl_config::effective::decode_effective_snapshot(&raw).unwrap(),
        effective
    );
}

// T41 T08 T14 (ruling 8): the raised deadline is what a snapshot restates,
// and it re-resolves; a declared one may reach the first-build bound only.
#[test]
fn an_undeclared_deadline_snapshot_decodes_and_the_bound_caps_a_declared_one() {
    let (mut deployment, host) = fixture();
    deployment
        .as_object_mut()
        .unwrap()
        .remove("request_deadline");
    let effective = resolve_effective(&deployment, &host).unwrap();
    assert_eq!(effective.request_deadline_ms, TENSORFOLD_FIRST_BUILD_MS);
    let raw = serde_json::to_value(&effective).unwrap().to_string();
    assert_eq!(
        capyctl_config::effective::decode_effective_snapshot(&raw).unwrap(),
        effective
    );
    let (mut deployment, host) = fixture();
    deployment["request_deadline"] = "1801s".into();
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert_eq!(error.path, "request_deadline", "{error}");
}

// T41 T21 (ADR 0023 §6): an undeclared residency on a TensorFold profile is
// restart_only even where the profile leaves deep parking on, and the
// TensorFold-off booleans may be stated as false.
#[test]
fn an_undeclared_residency_is_restart_only() {
    let (mut deployment, mut host) = fixture();
    host["runtime_profiles"]["local"]["security"]["deep_park"] = "enabled".into();
    deployment.as_object_mut().unwrap().remove("residency");
    deployment["engine_config"]["trust_remote_code"] = false.into();
    deployment["engine_config"]["language_model_only"] = false.into();
    let effective = resolve_effective(&deployment, &host).unwrap();
    assert_eq!(
        effective.residency,
        capyctl_config::effective::Residency::RestartOnly
    );
}
