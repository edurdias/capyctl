//! ADR 0014 amendment A15 (owner decision 2026-10-03): SGLang does not park a
//! deployment that runs speculative decoding. Its park releases the draft
//! model's weights with the target's, and its deep wake reloads them from the
//! target's checkpoint, so the draft would wake without its weights. Such a
//! deployment defaults to `restart_only`, and one that asks to park is refused
//! when it is resolved. CPU-only resolution tests; none of this qualifies an
//! engine recipe (SPEC §18).

use capyctl_config::effective::{resolve_effective_with_checkpoint, CheckpointFacts, Residency};
use capyctl_config::ConfigErrorCode;
use capyctl_domain::launch::SettingSource;
use serde_json::{json, Value};

const GIB: i64 = 1 << 30;

fn weights() -> CheckpointFacts {
    CheckpointFacts {
        weights_bytes: Some(GIB),
        ..Default::default()
    }
}

/// The fixture deployment on `engine`, with `extra_args` and no stated
/// residency, so the host's default applies.
fn deployment(engine: &str, extra_args: Value) -> (Value, Value) {
    let all: Value =
        serde_json::from_str(include_str!("fixtures/f2-deployment.json")).expect("fixture JSON");
    let (mut deployment, mut host) = (all["deployment"].clone(), all["host"].clone());
    let object = deployment.as_object_mut().unwrap();
    object.remove("resources");
    object.remove("residency");
    let profile = &mut host["runtime_profiles"]["local"];
    profile["engine"] = json!(engine);
    profile["args"] = json!([]);
    profile["security"]["approved_options"] =
        json!(["--speculative-config", "--speculative-draft-model-path"]);
    profile["security"]["approved_paths"] = json!(["/srv/drafters"]);
    if engine == "sglang" {
        profile["security"]["admin_credential_ref"] = json!("secret://engine-admin");
    }
    deployment["engine_config"]["memory"] = json!({"request": "40GiB", "kv_cache": "8GiB"});
    if extra_args.as_array().is_some_and(|args| !args.is_empty()) {
        deployment["engine_config"]["accept_extra_args"] = json!(true);
        deployment["engine_config"]["extra_args"] = extra_args;
    }
    (deployment, host)
}

fn dflash() -> Value {
    json!([
        "--speculative-draft-model-path",
        "/srv/drafters/d",
        "--speculative-algorithm",
        "DFLASH"
    ])
}

// T14 T21
#[test]
fn a_speculative_sglang_deployment_defaults_to_restart_only() {
    let (speculative, host) = deployment("sglang", dflash());
    let effective = resolve_effective_with_checkpoint(&speculative, &host, weights()).unwrap();
    assert_eq!(effective.residency, Residency::RestartOnly);
    let provenance = serde_json::to_value(&effective.engine_config).unwrap()["provenance"].clone();
    assert_eq!(
        provenance["residency"],
        json!(SettingSource::CapyctlDefault)
    );
    // The `=` spelling and an abbreviation name the same option.
    let (spelled, host) = deployment(
        "sglang",
        json!([
            "--speculative-draft-model-path=/srv/drafters/d",
            "--speculative-algo=NGRAM"
        ]),
    );
    let effective = resolve_effective_with_checkpoint(&spelled, &host, weights()).unwrap();
    assert_eq!(effective.residency, Residency::RestartOnly);
}

// T14 T21: without speculative decoding SGLang still parks by default, and
// vLLM's speculative deployments are not affected.
#[test]
fn other_deployments_keep_the_parking_default() {
    let (plain, host) = deployment("sglang", json!([]));
    let effective = resolve_effective_with_checkpoint(&plain, &host, weights()).unwrap();
    assert_eq!(effective.residency, Residency::Deep);
    let (vllm, host) = deployment(
        "vllm",
        json!([
            "--speculative-config",
            r#"{"method":"dflash","model":"/srv/drafters/d","num_speculative_tokens":7}"#
        ]),
    );
    let effective = resolve_effective_with_checkpoint(&vllm, &host, weights()).unwrap();
    assert_eq!(effective.residency, Residency::Deep);
}

// T14 T21: a speculative SGLang deployment that asks to park is refused with
// the reason, before anything is launched.
#[test]
fn a_speculative_sglang_deployment_that_asks_to_park_is_refused() {
    for residency in ["deep", "host_backed"] {
        let (mut speculative, host) = deployment("sglang", dflash());
        speculative["residency"] = json!(residency);
        let error = resolve_effective_with_checkpoint(&speculative, &host, weights()).unwrap_err();
        assert_eq!(
            error.code,
            ConfigErrorCode::UnsupportedCombination,
            "{error}"
        );
        assert_eq!(error.path, "residency");
        assert!(
            error.to_string().contains("speculative decoding")
                && error.to_string().contains("restart_only"),
            "{error}"
        );
    }
    let (mut stated, host) = deployment("sglang", dflash());
    stated["residency"] = json!("restart_only");
    let effective = resolve_effective_with_checkpoint(&stated, &host, weights()).unwrap();
    assert_eq!(effective.residency, Residency::RestartOnly);
}
