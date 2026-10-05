//! ADR 0014 amendments A15 and A17: an SGLang deployment that runs
//! speculative decoding parks with its weights resident. SGLang's weight
//! release takes the draft model's weights with the target's, and its disk
//! reload would load the target's checkpoint into the draft, so the park
//! releases the KV cache alone (`weight_restore: resident`). `host_backed` is
//! refused: the draft model has no host-RAM copy. CPU-only resolution tests; none of this qualifies an
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

/// The SGLang launch settings an effective deployment renders.
fn sglang_settings(effective: &capyctl_config::effective::EffectiveDeployment) -> Value {
    serde_json::to_value(&effective.engine_config).unwrap()
}

// T14 T21: amendment A17 parks it with its weights resident.
#[test]
fn a_speculative_sglang_deployment_parks_with_its_weights_resident() {
    let (speculative, host) = deployment("sglang", dflash());
    let effective = resolve_effective_with_checkpoint(&speculative, &host, weights()).unwrap();
    assert_eq!(effective.residency, Residency::Deep);
    let settings = sglang_settings(&effective);
    assert_eq!(settings["memory_saver"], json!(true));
    assert_eq!(settings["cpu_weight_backup"], json!(false));
    assert_eq!(settings["weight_restore"], json!("resident"));
    assert_eq!(
        settings["provenance"]["residency"],
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
    assert_eq!(effective.residency, Residency::Deep);
    assert_eq!(
        sglang_settings(&effective)["weight_restore"],
        json!("resident")
    );
    // Stated, `deep` is the same park.
    let (mut stated, host) = deployment("sglang", dflash());
    stated["residency"] = json!("deep");
    let effective = resolve_effective_with_checkpoint(&stated, &host, weights()).unwrap();
    assert_eq!(
        sglang_settings(&effective)["weight_restore"],
        json!("resident")
    );
}

// T14 T21: without speculative decoding SGLang still parks by default, and
// vLLM's speculative deployments are not affected.
#[test]
fn other_deployments_keep_the_parking_default() {
    let (plain, host) = deployment("sglang", json!([]));
    let effective = resolve_effective_with_checkpoint(&plain, &host, weights()).unwrap();
    assert_eq!(effective.residency, Residency::Deep);
    assert_eq!(
        sglang_settings(&effective)["weight_restore"],
        json!("disk_reload")
    );
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

// T14 T21: `host_backed` is still refused: its wake restores the target's
// weights from host RAM and the draft model has no copy there.
#[test]
fn a_speculative_sglang_deployment_that_asks_for_host_backed_is_refused() {
    let (mut speculative, host) = deployment("sglang", dflash());
    speculative["residency"] = json!("host_backed");
    let error = resolve_effective_with_checkpoint(&speculative, &host, weights()).unwrap_err();
    assert_eq!(
        error.code,
        ConfigErrorCode::UnsupportedCombination,
        "{error}"
    );
    assert_eq!(error.path, "residency");
    assert!(
        error.to_string().contains("speculative decoding") && error.to_string().contains("deep"),
        "{error}"
    );
    let (mut stated, host) = deployment("sglang", dflash());
    stated["residency"] = json!("restart_only");
    let effective = resolve_effective_with_checkpoint(&stated, &host, weights()).unwrap();
    assert_eq!(effective.residency, Residency::RestartOnly);
}
