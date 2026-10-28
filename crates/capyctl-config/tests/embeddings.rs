//! ADR 0036 §1, §2, §7: an `api: embeddings` deployment, how it resolves and
//! what refuses it. CPU-only tests; none of this is qualification of a native
//! engine recipe.

use std::collections::BTreeSet;

use capyctl_config::effective::{
    decode_effective_snapshot, deployment_command_fingerprint, resolve_effective_with_checkpoint,
    CheckpointFacts, DeploymentApi, EffectiveDeployment, Residency,
};
use capyctl_config::embeddings::{
    certification, check_residency, default_residency, support, Certification, EmbeddingSupport,
};
use capyctl_config::engine_policy::{
    validate_extra_args, Engine, ExtraArgsContext, ProfileArgError,
};
use capyctl_config::instances::parse_instance_spec;
use capyctl_config::lmcache::{plan, LmcacheMode, LmcacheOffReason, LmcacheState, PlanInputs};
use capyctl_config::{ConfigError, ConfigErrorCode};
use serde_json::{json, Value};

fn fixture() -> (Value, Value) {
    let all: Value = serde_json::from_str(include_str!("fixtures/f2-deployment.json")).unwrap();
    let (deployment, mut host) = (all["deployment"].clone(), all["host"].clone());
    host["runtime_profiles"]["local"]["args"] = json!([]);
    (deployment, host)
}

/// The fixture on `engine` at the build `fingerprint`, as an embeddings
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
    deployment["api"] = "embeddings".into();
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

fn with_extra_args(deployment: &Value, args: &[&str]) -> Value {
    let mut deployment = deployment.clone();
    deployment["engine_config"]["accept_extra_args"] = true.into();
    deployment["engine_config"]["extra_args"] = json!(args);
    deployment
}

// T47 T14 (ADR 0036 §1): `api: embeddings` resolves into the revision and its
// identity and round-trips through the frozen snapshot; the same checkpoint
// served as chat is another revision and another command.
#[test]
fn the_api_resolves_and_round_trips() {
    let (embeddings, host) = on("vllm", "0.32.0");
    let effective = resolve(&embeddings, &host).unwrap();
    assert_eq!(effective.api, DeploymentApi::Embeddings);
    let text = serde_json::to_string(&effective).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap()["api"],
        "embeddings"
    );
    assert_eq!(decode_effective_snapshot(&text).unwrap(), effective);
    let mut forged: Value = serde_json::from_str(&text).unwrap();
    forged.as_object_mut().unwrap().remove("api");
    assert!(decode_effective_snapshot(&forged.to_string()).is_err());

    let mut chat = embeddings.clone();
    chat.as_object_mut().unwrap().remove("api");
    let chat_effective = resolve(&chat, &host).unwrap();
    assert_ne!(
        chat_effective.recipe_fingerprint,
        effective.recipe_fingerprint
    );
    let command = |document: &Value| deployment_command_fingerprint(document, 300_000).unwrap();
    assert_ne!(command(&chat), command(&embeddings));
    assert_eq!(DeploymentApi::Embeddings.name(), "embeddings");
}

// T47 (ADR 0036 §1, owner decision 2026-10-27 question 1): every version of
// vLLM, SGLang and llama.cpp resolves; the qualified ones are certified and
// the others uncertified, never refused. TensorFold has no embeddings route
// and is `capability_missing:embeddings`.
#[test]
fn every_engine_version_with_a_route_resolves_and_tensorfold_is_refused() {
    for (engine, fingerprint, certified) in [
        ("vllm", "0.32.0", true),
        ("vllm", "0.29.0", false),
        ("vllm", "0.31.0", false),
        ("vllm", "vllm-build-1", false),
        ("sglang", "0.5.22", true),
        ("sglang", "0.5.20", false),
        ("sglang", "0.5.21", false),
        ("llamacpp", "0.7.0+620d222", true),
        ("llamacpp", "0.6.0+d812350", false),
    ] {
        let (deployment, host) = on(engine, fingerprint);
        let effective = resolve(&deployment, &host)
            .unwrap_or_else(|error| panic!("{engine} {fingerprint}: {error}"));
        assert_eq!(effective.api, DeploymentApi::Embeddings);
        let expected = if certified {
            Certification::Certified
        } else {
            Certification::Uncertified
        };
        assert_eq!(
            certification(effective.profile.engine, fingerprint),
            expected,
            "{engine} {fingerprint}"
        );
    }
    let (deployment, host) = on("tensorfold", "1.1.4");
    let error = resolve(&deployment, &host).unwrap_err();
    refused_at(&error, "api");
    assert!(
        error.detail.starts_with("capability_missing:embeddings:"),
        "{error}"
    );
}

// T47 (ADR 0036 §7.1, §7.2): one machine only; `host_backed` is refused from
// the document alone, as every path that validates it does.
#[test]
fn a_topology_and_host_backed_are_refused() {
    let (base, host) = on("vllm", "0.32.0");
    let mut deployment = base.clone();
    deployment["topology"] = json!({"tensor_parallel": 2});
    deployment["placement"] = json!({"hosts": ["host-a", "host-b"]});
    refused_at(&parse_instance_spec(&deployment).unwrap_err(), "topology");
    refused_at(&resolve(&deployment, &host).unwrap_err(), "topology");
    for engine in ["vllm", "sglang", "llamacpp"] {
        let (mut deployment, host) = on(engine, "0.32.0");
        deployment["residency"] = "host_backed".into();
        refused_at(&parse_instance_spec(&deployment).unwrap_err(), "residency");
        refused_at(&resolve(&deployment, &host).unwrap_err(), "residency");
    }
}

// T47 (ADR 0036 §7.2, owner decision 2026-10-27 question 2): vLLM and SGLang
// park `deep` when no residency is stated; llama.cpp restarts and refuses a
// stated `deep`.
#[test]
fn vllm_and_sglang_park_deep_and_llamacpp_restarts() {
    for (engine, fingerprint) in [("vllm", "0.32.0"), ("vllm", "0.30.0"), ("sglang", "0.5.22")] {
        let (deployment, host) = on(engine, fingerprint);
        let effective = resolve(&deployment, &host).unwrap();
        assert_eq!(effective.residency, Residency::Deep, "{engine}");
        let mut stated = deployment.clone();
        stated["residency"] = "deep".into();
        assert_eq!(resolve(&stated, &host).unwrap().residency, Residency::Deep);
    }
    let (deployment, host) = on("llamacpp", "0.7.0+620d222");
    assert_eq!(
        resolve(&deployment, &host).unwrap().residency,
        Residency::RestartOnly
    );
    let mut deep = deployment.clone();
    deep["residency"] = "deep".into();
    let error = resolve(&deep, &host).unwrap_err();
    refused_at(&error, "residency");
    assert!(
        error.detail.starts_with("capability_missing:deep_park"),
        "{error}"
    );
}

// T47 (ADR 0036 §7.2): while an engine's support data has embeddings parking
// off (its EM9 row failed), its deployments resolve `restart_only` and a
// stated `deep` is `capability_missing:deep_park`.
#[test]
fn support_data_with_parking_off_restarts_and_refuses_deep() {
    for engine in [Engine::Vllm, Engine::Sglang] {
        assert_eq!(support(engine), Some(EmbeddingSupport { deep_park: true }));
        let off = Some(EmbeddingSupport { deep_park: false });
        assert_eq!(default_residency(off, true), Residency::RestartOnly);
        let error = check_residency(engine, Some(Residency::Deep), off).unwrap_err();
        refused_at(&error, "residency");
        assert!(error.detail.starts_with("capability_missing:deep_park"));
        check_residency(engine, Some(Residency::RestartOnly), off).unwrap();
        check_residency(engine, None, off).unwrap();
        check_residency(engine, Some(Residency::Deep), support(engine)).unwrap();
    }
    for engine in [Engine::Llamacpp, Engine::Custom] {
        assert_eq!(support(engine), None);
        assert_eq!(default_residency(None, true), Residency::RestartOnly);
    }
    // A profile that turns deep parking off restarts too.
    assert_eq!(
        default_residency(support(Engine::Vllm), false),
        Residency::RestartOnly
    );
}

// T47 T14 (ADR 0036 §1, amending ADR 0024 §1): an embeddings deployment gets
// no tool-call or reasoning parser.
#[test]
fn a_stated_parser_is_refused() {
    let (base, host) = on("sglang", "0.5.22");
    let mut deployment = base.clone();
    deployment["engine_config"]["sglang"] = json!({"reasoning_parser": "qwen3"});
    let error = resolve(&deployment, &host).unwrap_err();
    refused_at(&error, "engine_config.sglang.reasoning_parser");
    assert!(error.detail.contains("embeddings deployment"), "{error}");
    let deployment = with_extra_args(&base, &["--tool-call-parser", "qwen25"]);
    refused_at(
        &resolve(&deployment, &host).unwrap_err(),
        "engine_config.extra_args",
    );
    let effective = resolve(&base, &host).unwrap();
    assert_eq!(
        capyctl_config::parsers::parsers_on_remote_host(&effective),
        None
    );
}

// T47 T45 (ADR 0036 §2): LMCache is off with `embeddings_unsupported`, and
// `lmcache: required` is refused `lmcache_unavailable:embeddings_unsupported`.
#[test]
fn lmcache_is_off_and_required_is_refused() {
    let (mut deployment, host) = on("vllm", "0.32.0");
    deployment["lmcache"] = "required".into();
    let error = resolve(&deployment, &host).unwrap_err();
    refused_at(&error, "lmcache");
    assert!(
        error
            .detail
            .starts_with("lmcache_unavailable:embeddings_unsupported"),
        "{error}"
    );
    let inputs = PlanInputs {
        mode: LmcacheMode::Auto,
        api: DeploymentApi::Embeddings,
        ..PlanInputs::for_profile(Engine::Vllm, "0.32.0", Residency::Deep)
    };
    let LmcacheState::Off(off) = plan(&inputs).state else {
        panic!("LMCache must be off");
    };
    assert_eq!(off.reason, LmcacheOffReason::EmbeddingsUnsupported);
    assert_eq!(off.reason.code(), "embeddings_unsupported");
    assert_eq!(
        LmcacheOffReason::parse("embeddings_unsupported"),
        Some(LmcacheOffReason::EmbeddingsUnsupported)
    );
}

// T47 T14 (ADR 0036 §2): vLLM's `--runner` and `--convert` and SGLang's
// `--is-embedding` are reserved on every deployment, chat and embeddings.
#[test]
fn the_embedding_switches_are_reserved_on_every_deployment() {
    for (engine, fingerprint, option, value) in [
        ("vllm", "0.32.0", "--runner", "pooling"),
        ("vllm", "0.32.0", "--convert", "embed"),
        ("sglang", "0.5.22", "--is-embedding", "true"),
    ] {
        let (embeddings, host) = on(engine, fingerprint);
        let mut chat = embeddings.clone();
        chat.as_object_mut().unwrap().remove("api");
        for deployment in [embeddings, chat] {
            let stated = with_extra_args(&deployment, &[option, value]);
            let error = resolve(&stated, &host).unwrap_err();
            refused_at(&error, "engine_config.extra_args");
            assert!(error.detail.contains("reserved"), "{option}: {error}");
        }
    }
}

fn llamacpp_extra(api: DeploymentApi, list: &[&str]) -> Result<(), ProfileArgError> {
    let none = BTreeSet::new();
    let args: Vec<String> = list.iter().map(|item| (*item).to_owned()).collect();
    validate_extra_args(
        &args,
        &ExtraArgsContext {
            engine: Engine::Llamacpp,
            api,
            sleep_mode: false,
            approved_options: &none,
            approved_paths: &[],
            checkpoint_root: None,
            host_fixed: &none,
        },
    )
}

// T47 T42 (ADR 0036 §2, amending ADR 0029 §6): llama.cpp's `--pooling` is
// ordinary on an embeddings deployment except `none`, and stays reserved on
// chat and decisions deployments; `--embedding(s)` and `--rerank` stay
// reserved everywhere, since CapyCTL renders `--embeddings` itself.
#[test]
fn llamacpp_pooling_is_ordinary_on_embeddings_only() {
    use DeploymentApi::*;
    for pooling in ["mean", "cls", "last", "rank"] {
        llamacpp_extra(Embeddings, &["--pooling", pooling]).unwrap();
        for api in [Chat, Decisions] {
            assert!(matches!(
                llamacpp_extra(api, &["--pooling", pooling]),
                Err(ProfileArgError::Reserved(_))
            ));
        }
    }
    assert!(matches!(
        llamacpp_extra(Embeddings, &["--pooling", "none"]),
        Err(ProfileArgError::PoolingNone(_))
    ));
    for option in ["--embedding", "--embeddings", "--rerank", "--reranking"] {
        assert!(matches!(
            llamacpp_extra(Embeddings, &[option]),
            Err(ProfileArgError::Reserved(_))
        ));
    }
    assert_eq!(capyctl_config::llamacpp::EMBEDDINGS_OPTION, "--embeddings");
    // Host-fixed arguments are no deployment's: `--pooling` stays reserved.
    assert!(matches!(
        capyctl_config::engine_policy::validate_profile_args(
            Engine::Llamacpp,
            &["--pooling".to_owned(), "mean".to_owned()],
            false
        ),
        Err(ProfileArgError::Reserved(_))
    ));
    // Through resolution.
    let (base, host) = on("llamacpp", "0.7.0+620d222");
    resolve(&with_extra_args(&base, &["--pooling", "mean"]), &host).unwrap();
    refused_at(
        &resolve(&with_extra_args(&base, &["--pooling", "none"]), &host).unwrap_err(),
        "engine_config.extra_args",
    );
}

// T47 (ADR 0036 §1): an unknown `api` value is refused.
#[test]
fn an_unknown_api_is_refused() {
    let (mut deployment, host) = on("vllm", "0.32.0");
    deployment["api"] = "embedding".into();
    let error = resolve(&deployment, &host).unwrap_err();
    refused_at(&error, "api");
}
