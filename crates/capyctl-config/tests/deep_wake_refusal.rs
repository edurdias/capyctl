//! SPEC §§6.2, 9.1 / ADR 0010: a parking residency the engine cannot honor
//! for a checkpoint's model family is refused `capability_missing:deep_park`
//! (`EffectiveDeployment::deep_wake_refusal`). CPU-only tests; none of this is
//! qualification of a native engine recipe.

use capyctl_config::effective::{resolve_effective, EffectiveDeployment, Residency};
use capyctl_domain::launch::LaunchSettings;
use serde_json::{json, Value};

fn resolve(engine: &str, residency: &str) -> EffectiveDeployment {
    let all: Value = serde_json::from_str(include_str!("fixtures/f2-deployment.json")).unwrap();
    let (mut deployment, mut host) = (all["deployment"].clone(), all["host"].clone());
    host["runtime_profiles"]["local"]["args"] = json!([]);
    if engine == "sglang" {
        let profile = &mut host["runtime_profiles"]["local"];
        profile["engine"] = "sglang".into();
        profile["security"]["admin_credential_ref"] = "secret://admin-key".into();
    }
    deployment["residency"] = residency.into();
    resolve_effective(&deployment, &host).expect("resolves")
}

fn checkpoint(config: Value) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.json"), config.to_string()).unwrap();
    dir
}

fn on(mut effective: EffectiveDeployment, dir: &tempfile::TempDir) -> EffectiveDeployment {
    effective.model.resolved_path = Some(dir.path().to_str().unwrap().to_owned());
    effective
}

/// The `openai/gpt-oss-20b` checkpoint's `config.json` names.
fn gpt_oss() -> Value {
    json!({"architectures": ["GptOssForCausalLM"], "model_type": "gpt_oss"})
}

/// gpt-oss cannot be woken from a park on vLLM 0.30.0 (weights fail to reload,
/// the engine then decodes garbage) nor on SGLang 0.5.21 (the disk reload
/// raises a TypeError): `deep` and `host_backed` are refused on both engines,
/// recognised by `model_type` or, without one, by architecture.
// T21 T22
#[test]
fn gpt_oss_refuses_both_parking_tiers_on_vllm_and_sglang() {
    for config in [
        gpt_oss(),
        json!({"architectures": ["GptOssForCausalLM"]}),
        json!({"model_type": "gpt_oss"}),
    ] {
        let dir = checkpoint(config.clone());
        for engine in ["vllm", "sglang"] {
            let deep = on(resolve(engine, "deep"), &dir);
            assert_eq!(deep.residency, Residency::Deep);
            assert_eq!(
                deep.deep_wake_refusal(),
                Some("capability_missing:deep_park"),
                "{engine} {config}"
            );
            let mut host_backed = deep.clone();
            host_backed.residency = Residency::HostBacked;
            assert_eq!(
                host_backed.deep_wake_refusal(),
                Some("capability_missing:deep_park"),
                "{engine} {config}"
            );
        }
    }
}

/// `restart_only` never parks, so gpt-oss serves; another family parks as
/// before; a checkpoint this machine cannot read refuses nothing (its host
/// decides); and an SGLang park that keeps the weights resident reloads
/// nothing, so it is not refused (ADR 0014 amendment A17).
// T21
#[test]
fn the_gpt_oss_rule_leaves_restart_only_other_families_and_resident_parks() {
    let dir = checkpoint(gpt_oss());
    for engine in ["vllm", "sglang"] {
        let restart = on(resolve(engine, "restart_only"), &dir);
        assert_eq!(restart.deep_wake_refusal(), None, "{engine}");
    }
    let qwen = checkpoint(json!({"architectures": ["Qwen3ForCausalLM"], "model_type": "qwen3"}));
    for engine in ["vllm", "sglang"] {
        assert_eq!(
            on(resolve(engine, "deep"), &qwen).deep_wake_refusal(),
            None,
            "{engine}"
        );
    }
    let mut unseen = resolve("vllm", "deep");
    unseen.model.resolved_path = Some("/nonexistent/capyctl-test/gpt-oss".into());
    assert_eq!(unseen.deep_wake_refusal(), None);

    let mut resident = on(resolve("sglang", "deep"), &dir);
    let LaunchSettings::Sglang(settings) = &mut resident.engine_config else {
        panic!("an SGLang launch");
    };
    settings.weight_restore = "resident".into();
    assert_eq!(resident.deep_wake_refusal(), None);
}
