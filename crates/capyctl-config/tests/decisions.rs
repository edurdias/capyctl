//! ADR 0030 §1, §7: a deployment's `api`, how it resolves and what refuses it.
//! CPU-only tests; none of this is qualification of a native engine recipe.

use capyctl_config::effective::{
    decode_effective_snapshot, deployment_command_fingerprint, resolve_effective,
    resolve_effective_with_checkpoint, CheckpointFacts, DeploymentApi, EffectiveDeployment,
    Residency,
};
use capyctl_config::instances::parse_instance_spec;
use capyctl_config::{ConfigError, ConfigErrorCode};
use capyctl_domain::launch::SettingSource;
use serde_json::{json, Value};

fn fixture() -> (Value, Value) {
    let all: Value = serde_json::from_str(include_str!("fixtures/f2-deployment.json")).unwrap();
    let (deployment, mut host) = (all["deployment"].clone(), all["host"].clone());
    host["runtime_profiles"]["local"]["args"] = json!([]);
    (deployment, host)
}

/// The fixture on `engine` at the build `fingerprint`, as a decisions
/// deployment that states no residency.
fn on(engine: &str, fingerprint: &str) -> (Value, Value) {
    let (mut deployment, mut host) = fixture();
    let profile = &mut host["runtime_profiles"]["local"];
    profile["engine"] = engine.into();
    profile["build_fingerprint"] = fingerprint.into();
    match engine {
        "sglang" => {
            profile["security"]["admin_credential_ref"] = "secret://admin-key".into();
        }
        "llamacpp" => {
            profile["executable"] = "/opt/llama.cpp/bin/llama-server".into();
            profile["security"]["deep_park"] = "disabled".into();
            deployment["engine_config"] = json!({"context_length": 8192});
        }
        "tensorfold" => {
            profile["security"]["deep_park"] = "disabled".into();
        }
        _ => {}
    }
    deployment["api"] = "decisions".into();
    deployment.as_object_mut().unwrap().remove("residency");
    (deployment, host)
}

/// ADR 0029 §9: a llama.cpp deployment resolves once its GGUF is measured.
fn measured() -> CheckpointFacts {
    use capyctl_domain::gguf::{GgufFacts, GgufKv, GgufKvShape};
    let values = 32 * 8 * 128;
    CheckpointFacts {
        weights_bytes: Some(0),
        gguf: Some(GgufFacts {
            weights_bytes: 0,
            training_context: Some(32768),
            kv: GgufKv::Attention(GgufKvShape {
                layers: 32,
                k_values: values,
                v_values: values,
                v_values_padded: values,
            }),
        }),
        ..CheckpointFacts::default()
    }
}

fn resolve(deployment: &Value, host: &Value) -> Result<EffectiveDeployment, ConfigError> {
    resolve_effective_with_checkpoint(deployment, host, measured())
}

fn refused_at(error: &ConfigError, path: &str) {
    assert_eq!(
        (error.code, error.path.as_str()),
        (ConfigErrorCode::UnsupportedCombination, path),
        "{error}"
    );
}

// T43 T14 (ADR 0030 §1): `api: decisions` resolves into the revision and its
// identity, round-trips through the frozen snapshot, and defaults the
// residency to restart_only; an undeclared or `chat` api is the default and
// leaves the revision exactly as it was.
#[test]
fn the_api_resolves_round_trips_and_defaults_to_chat() {
    let (decisions, host) = on("sglang", "0.5.22");
    let effective = resolve(&decisions, &host).unwrap();
    assert_eq!(effective.api, DeploymentApi::Decisions);
    assert_eq!(effective.residency, Residency::RestartOnly);
    assert_eq!(
        serde_json::to_value(&effective.engine_config).unwrap()["provenance"]["residency"],
        serde_json::to_value(SettingSource::CapyctlDefault).unwrap()
    );
    let text = serde_json::to_string(&effective).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap()["api"],
        "decisions"
    );
    assert_eq!(decode_effective_snapshot(&text).unwrap(), effective);
    // A snapshot that drops the api claims another revision.
    let mut forged: Value = serde_json::from_str(&text).unwrap();
    forged.as_object_mut().unwrap().remove("api");
    assert!(decode_effective_snapshot(&forged.to_string()).is_err());

    let mut chat = decisions.clone();
    chat.as_object_mut().unwrap().remove("api");
    let omitted = resolve(&chat, &host).unwrap();
    assert_eq!(omitted.api, DeploymentApi::Chat);
    let text = serde_json::to_string(&omitted).unwrap();
    assert!(serde_json::from_str::<Value>(&text)
        .unwrap()
        .get("api")
        .is_none());
    assert_eq!(decode_effective_snapshot(&text).unwrap(), omitted);
    // The same checkpoint served as chat and as decisions are two revisions.
    assert_ne!(omitted.recipe_fingerprint, effective.recipe_fingerprint);
    chat["api"] = "chat".into();
    let stated = resolve(&chat, &host).unwrap();
    assert_eq!(stated, omitted);
    // And two commands; `api: chat` is the command that omits it.
    let command = |document: &Value| deployment_command_fingerprint(document, 300_000).unwrap();
    let mut without = chat.clone();
    without.as_object_mut().unwrap().remove("api");
    assert_eq!(command(&chat), command(&without));
    assert_ne!(command(&chat), command(&decisions));

    chat["api"] = "completions".into();
    refused_at(&resolve(&chat, &host).unwrap_err(), "api");
}

// T43 T39 (ADR 0030 §1): every revision frozen before `api` existed keeps the
// recipe fingerprint it was frozen with, and stating `api: chat` changes
// nothing about it.
#[test]
fn existing_revisions_keep_their_fingerprints() {
    for fixture in [
        include_str!("fixtures/effective-vllm-golden.json"),
        include_str!("fixtures/effective-sglang-golden.json"),
    ] {
        let source: Value = serde_json::from_str(fixture).unwrap();
        let frozen = &source["effective"];
        assert!(frozen.get("api").is_none());
        let mut deployment = source["input"]["deployment"].clone();
        let host = &source["input"]["host"];
        let effective = resolve_effective(&deployment, host).unwrap();
        assert_eq!(effective.recipe_fingerprint, frozen["recipe_fingerprint"]);
        assert_eq!(
            decode_effective_snapshot(&frozen.to_string()).unwrap(),
            effective
        );
        deployment["api"] = "chat".into();
        assert_eq!(
            resolve_effective(&deployment, host)
                .unwrap()
                .recipe_fingerprint,
            frozen["recipe_fingerprint"]
        );
    }
}

// T43 (ADR 0030 §1, §6, amended 2026-10-21): only llama.cpp 0.7.0 or newer and
// SGLang 0.5.22 or newer serve decision models; vLLM on every version (0.32.0
// reads Clef and Clef-Flash from their text output, not their decision head),
// TensorFold, older releases and a build that names no version are
// `capability_missing:decisions`, before anything else about the deployment
// is resolved.
#[test]
fn an_engine_that_cannot_serve_decisions_is_refused() {
    for (engine, fingerprint) in [
        ("vllm", "0.31.0"),
        ("vllm", "0.32.0"),
        ("vllm", "0.33.0"),
        ("vllm", "vllm-build-1"),
        ("tensorfold", "1.0.12"),
        ("llamacpp", "0.6.0+d812350"),
        ("sglang", "0.5.21"),
        ("sglang", "0.5.22rc1"),
        ("sglang", "Python 3.12.3"),
        ("sglang", "sglang-build-1"),
    ] {
        let (deployment, host) = on(engine, fingerprint);
        let error = resolve(&deployment, &host).unwrap_err();
        refused_at(&error, "api");
        assert!(
            error.detail.starts_with("capability_missing:decisions:"),
            "{engine} {fingerprint}: {error}"
        );
        if engine == "vllm" {
            assert!(error.detail.contains("decision head"), "{error}");
        }
        // The same deployment serving chat is not refused for it.
        let mut chat = deployment.clone();
        chat.as_object_mut().unwrap().remove("api");
        if let Err(error) = resolve(&chat, &host) {
            assert!(
                !error.detail.contains("capability_missing:decisions"),
                "{engine}: {error}"
            );
        }
    }
    for (engine, fingerprint) in [
        ("llamacpp", "0.7.0+abc1234"),
        ("llamacpp", "0.8.1+abc1234"),
        ("sglang", "0.5.22"),
        ("sglang", "0.5.22.post1"),
        ("sglang", "0.6.0"),
    ] {
        let (deployment, host) = on(engine, fingerprint);
        let effective = resolve(&deployment, &host)
            .unwrap_or_else(|error| panic!("{engine} {fingerprint}: {error}"));
        assert_eq!(effective.api, DeploymentApi::Decisions);
        assert_eq!(effective.residency, Residency::RestartOnly);
    }
}

// T43 (ADR 0030 §7): a decisions deployment runs on one machine and restarts
// instead of parking: a topology (any shape) and a declared parking residency
// are refused from the document alone, as every path that validates it does.
#[test]
fn decisions_runs_on_one_machine_and_never_parks() {
    let (base, host) = on("sglang", "0.5.22");
    for topology in [
        json!({"tensor_parallel": 2}),
        json!({"tensor_parallel": 1, "pipeline_parallel": 1}),
    ] {
        let mut deployment = base.clone();
        deployment["topology"] = topology.clone();
        deployment["placement"] = json!({"hosts": ["host-a", "host-b"]});
        refused_at(&parse_instance_spec(&deployment).unwrap_err(), "topology");
        refused_at(&resolve(&deployment, &host).unwrap_err(), "topology");
        // A chat deployment with the same shape is not refused for it.
        deployment.as_object_mut().unwrap().remove("api");
        if let Err(error) = parse_instance_spec(&deployment) {
            assert_ne!(error.path, "topology", "{topology}: {error}");
        }
    }
    for tier in ["deep", "host_backed"] {
        let mut deployment = base.clone();
        deployment["residency"] = tier.into();
        refused_at(&parse_instance_spec(&deployment).unwrap_err(), "residency");
        refused_at(&resolve(&deployment, &host).unwrap_err(), "residency");
    }
    let mut deployment = base.clone();
    deployment["residency"] = "restart_only".into();
    let effective = resolve(&deployment, &host).unwrap();
    assert_eq!(effective.residency, Residency::RestartOnly);
    // Stated, it is the operator's: no capyctl default is named for it.
    assert!(
        serde_json::to_value(&effective.engine_config).unwrap()["provenance"]
            .get("residency")
            .is_none()
    );
}

// T43 T14 (ADR 0030 §1, amending ADR 0024 §1): a decisions deployment gets no
// tool-call or reasoning parser. Auto-selection chooses none, even for a
// checkpoint whose family has defaults, and a parser the deployment states
// is refused at its path.
#[test]
fn decisions_has_no_parser() {
    let (base, host) = on("sglang", "0.5.22");
    for (field, value) in [
        ("tool_call_parser", "qwen25"),
        ("reasoning_parser", "qwen3"),
        ("tool_call_parser", "none"),
    ] {
        let mut deployment = base.clone();
        deployment["engine_config"]["sglang"] = json!({ field: value });
        refused_at(
            &resolve(&deployment, &host).unwrap_err(),
            &format!("engine_config.sglang.{field}"),
        );
    }
    for option in ["--reasoning-parser=qwen3", "--tool-call-parser"] {
        let mut deployment = base.clone();
        deployment["engine_config"]["accept_extra_args"] = true.into();
        deployment["engine_config"]["extra_args"] = json!([option, "qwen25"]);
        refused_at(
            &resolve(&deployment, &host).unwrap_err(),
            "engine_config.extra_args",
        );
    }
    // `auto`, the written default, chooses none.
    let mut deployment = base.clone();
    deployment["engine_config"]["sglang"] = json!({"tool_call_parser": "auto"});
    let mut decisions = resolve(&deployment, &host).unwrap();
    let mut chat_document = deployment.clone();
    chat_document.as_object_mut().unwrap().remove("api");
    let mut chat = resolve(&chat_document, &host).unwrap();
    // A Qwen3.5 checkpoint, whose family has both parsers.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.json"),
        json!({"architectures": ["Qwen3_5ForConditionalGeneration"], "model_type": "qwen3_5"})
            .to_string(),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("tokenizer_config.json"),
        json!({"chat_template": "<tool_call><function=f></function></tool_call><think>"})
            .to_string(),
    )
    .unwrap();
    let root = Some(dir.path().to_str().unwrap().to_owned());
    decisions.model.resolved_path = root.clone();
    chat.model.resolved_path = root;
    let chosen = capyctl_config::parsers::parsers_for_effective(&chat).expect("chat parsers");
    assert!(chosen.tool_call.name.is_some() && chosen.reasoning.name.is_some());
    assert_eq!(
        capyctl_config::parsers::parsers_for_effective(&decisions),
        None
    );
    assert_eq!(
        capyctl_config::parsers::parsers_on_remote_host(&decisions),
        None
    );
}
