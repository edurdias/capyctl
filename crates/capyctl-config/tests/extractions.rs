//! ADR 0041 §§1–2: extraction resolution and identity. CPU tests, not qualification.
use capyctl_config::custom::{CustomManifest, CustomProfile};
use capyctl_config::effective::{
    decode_effective_snapshot, resolve_effective_with_checkpoint, CheckpointFacts, DeploymentApi,
    InstallationDrift, Residency,
};
use capyctl_config::extractions::FieldType;
use capyctl_config::registration::{custom_profile_document, CustomProfileSpec};
use serde_json::{json, Value};

fn manifest(api: &str) -> Value {
    let mut value = json!({"kind":"custom_engine","schema_version":1,"api":api,"args":["--host","{host}","--port","{port}","--model","{model_dir}"]});
    if api == "extractions" {
        value["field_types"] = json!(["forecast", "text", "number"]);
    }
    value
}
fn fixture(api: &str) -> (Value, Value) {
    let all: Value = serde_json::from_str(include_str!("fixtures/f2-deployment.json")).unwrap();
    let (mut deployment, mut host) = (all["deployment"].clone(), all["host"].clone());
    host["runtime_profiles"]["local"] = custom_profile_document(&CustomProfileSpec {
        executable: "/opt/extract/bin/serve".into(),
        custom: CustomProfile::new(CustomManifest::from_value(manifest(api)).unwrap()),
        installation_drift: InstallationDrift::Warn,
        env: Default::default(),
        approved_env: vec![],
    });
    deployment["runtime_profile_revision"] = 1.into();
    deployment["request_deadline"] = "600s".into();
    deployment["api"] = api.into();
    deployment["engine_config"] = json!({});
    deployment.as_object_mut().unwrap().remove("residency");
    (deployment, host)
}
fn resolve(
    deployment: &Value,
    host: &Value,
) -> Result<capyctl_config::effective::EffectiveDeployment, capyctl_config::ConfigError> {
    resolve_effective_with_checkpoint(
        deployment,
        host,
        CheckpointFacts {
            weights_bytes: Some(3_000_000_000),
            ..Default::default()
        },
    )
}
// T50: extraction declarations survive frozen revisions and name a distinct recipe.
#[test]
fn custom_extractions_resolve_and_round_trip() {
    let (deployment, host) = fixture("extractions");
    let effective = resolve(&deployment, &host).unwrap();
    assert_eq!(effective.api, DeploymentApi::Extractions);
    assert_eq!(effective.residency, Residency::RestartOnly);
    assert_eq!(
        effective
            .profile
            .custom
            .as_ref()
            .unwrap()
            .manifest
            .field_types,
        Some(vec![
            FieldType::Text,
            FieldType::Number,
            FieldType::Forecast
        ])
    );
    assert_eq!(
        decode_effective_snapshot(&serde_json::to_string(&effective).unwrap()).unwrap(),
        effective
    );
}
// T50: the optional new manifest member changes no existing decisions bytes or digest.
#[test]
fn manifest_identity_is_canonical_and_legacy_digest_is_unchanged() {
    let legacy = CustomManifest::from_value(manifest("decisions")).unwrap();
    assert_eq!(
        legacy.digest(),
        "sha256:b9ecd0104f102c9a0e31783ef1cd919245a532a204dbfecbdb0e83fcc2db6973"
    );
    assert!(serde_json::to_value(&legacy)
        .unwrap()
        .get("field_types")
        .is_none());
    let first = CustomManifest::from_value(manifest("extractions")).unwrap();
    let mut ordered = manifest("extractions");
    ordered["field_types"] = json!(["text", "number", "forecast"]);
    assert_eq!(
        first.digest(),
        CustomManifest::from_value(ordered.clone())
            .unwrap()
            .digest()
    );
    ordered["field_types"] = json!(["text"]);
    assert_ne!(
        first.digest(),
        CustomManifest::from_value(ordered).unwrap().digest()
    );
}
// T50: unknown, empty, duplicate, missing and inapplicable declarations are refused.
#[test]
fn manifest_types_are_closed_and_required_only_for_extractions() {
    for value in [
        Value::Null,
        json!([]),
        json!(["text", "text"]),
        json!(["boolean"]),
    ] {
        let mut input = manifest("extractions");
        input["field_types"] = value;
        assert!(CustomManifest::from_value(input).unwrap_err().code == "custom_manifest_invalid");
    }
    let mut missing = manifest("extractions");
    missing.as_object_mut().unwrap().remove("field_types");
    assert!(CustomManifest::from_value(missing).is_err());
    for api in ["chat", "decisions", "embeddings"] {
        let mut input = manifest(api);
        input["field_types"] = json!(["text"]);
        assert!(CustomManifest::from_value(input).is_err());
    }
}
// T50: extraction documents cannot request topology, parking, or parsers.
#[test]
fn extraction_scope_is_refused_before_resolution() {
    for (key, value, expected) in [
        ("topology", json!({"tensor_parallel":2}), "topology"),
        ("residency", json!("deep"), "residency"),
        ("residency", json!("host_backed"), "residency"),
        (
            "engine_config",
            json!({"vllm":{"tool_call_parser":"hermes"}}),
            "engine_config.tool_call_parser",
        ),
        (
            "engine_config",
            json!({"sglang":{"reasoning_parser":"qwen3"}}),
            "engine_config.reasoning_parser",
        ),
    ] {
        let (mut deployment, host) = fixture("extractions");
        deployment[key] = value;
        let error = resolve(&deployment, &host).unwrap_err();
        assert_eq!(error.path, expected, "{error}");
        if key == "engine_config" {
            assert!(
                error.detail.contains("custom_field_unsupported:"),
                "{error}"
            );
        }
    }
}
// T50: manifest equality applies in both directions and all native versions stay closed.
#[test]
fn api_mismatch_and_native_engines_are_refused() {
    for api in ["chat", "decisions", "embeddings"] {
        let (mut deployment, host) = fixture("extractions");
        deployment["api"] = api.into();
        assert!(resolve(&deployment, &host)
            .unwrap_err()
            .detail
            .starts_with(&format!("capability_missing:{api}")));
        let (mut deployment, host) = fixture(api);
        deployment["api"] = "extractions".into();
        assert!(resolve(&deployment, &host)
            .unwrap_err()
            .detail
            .starts_with("capability_missing:extractions"));
    }
    use capyctl_config::engine_policy::Engine;
    for engine in [
        Engine::Vllm,
        Engine::Sglang,
        Engine::Llamacpp,
        Engine::Tensorfold,
        Engine::TensorrtLlm,
    ] {
        assert!(capyctl_config::extractions::check_engine(engine)
            .unwrap_err()
            .detail
            .starts_with("capability_missing:extractions"));
    }
    for (engine, version) in [("llamacpp", "0.7.0"), ("sglang", "0.5.22")] {
        let (mut deployment, mut host) = fixture("extractions");
        let profile = &mut host["runtime_profiles"]["local"];
        profile.as_object_mut().unwrap().remove("custom");
        profile["engine"] = engine.into();
        profile["build_fingerprint"] = version.into();
        deployment["engine_config"] = json!({});
        let error = resolve(&deployment, &host).unwrap_err();
        assert!(
            error.detail.starts_with("capability_missing:extractions"),
            "{error}"
        );
    }
}
// T50: frozen revisions without api keep their pre-extractions fingerprint.
#[test]
fn revisions_without_api_keep_their_fingerprint() {
    for text in [
        include_str!("fixtures/effective-vllm-golden.json"),
        include_str!("fixtures/effective-sglang-golden.json"),
    ] {
        let source: Value = serde_json::from_str(text).unwrap();
        let frozen = &source["effective"];
        assert!(frozen.get("api").is_none());
        let decoded = decode_effective_snapshot(&frozen.to_string()).unwrap();
        assert_eq!(
            decoded.recipe_fingerprint,
            frozen["recipe_fingerprint"].as_str().unwrap()
        );
        let effective = capyctl_config::effective::resolve_effective(
            &source["input"]["deployment"],
            &source["input"]["host"],
        )
        .unwrap();
        assert_eq!(effective, decoded);
    }
}
