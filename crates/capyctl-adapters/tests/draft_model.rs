//! ADR 0008 amendment 2026-10-08: CapyCTL hands a declared drafter's
//! directory (`model.draft`) to the engine's own draft-model option: SGLang's
//! `speculative_draft_model_path`, vLLM's `--speculative-config` `model`, and
//! TensorFold's `--drafter`. CPU tests only; they prove the rendering, not
//! that an engine starts with the drafter (SPEC §18).

use capyctl_adapters::sglang::{frozen_from_effective, SglangLaunch};
use capyctl_adapters::{tensorfold, vllm};
use capyctl_config::effective::{resolve_effective, EffectiveDeployment};
use capyctl_config::engine_policy::{vllm_args_with_draft, DRAFT_PATH_CONFLICT};
use serde_json::{json, Value};

const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn drafter_dir() -> String {
    format!("/srv/models/sources/http/{DIGEST}-tar")
}

fn speculative() -> String {
    json!({"method": "draft_model", "num_speculative_tokens": 4}).to_string()
}

/// The fixture deployment on `engine` with an http drafter, the operator's
/// speculation switch in its extras, and nothing approved for the drafter.
fn effective(engine: &str, draft: bool) -> EffectiveDeployment {
    let all: Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/f2-deployment.json"
    ))
    .expect("fixture JSON");
    let (mut deployment, mut host) = (all["deployment"].clone(), all["host"].clone());
    let profile = &mut host["runtime_profiles"]["local"];
    profile["engine"] = json!(engine);
    profile["args"] = json!([]);
    let extra = match engine {
        "sglang" => {
            profile["security"]["admin_credential_ref"] = json!("secret://engine-admin");
            json!(["--speculative-algorithm", "STANDALONE"])
        }
        "vllm" => {
            profile["security"]["approved_options"] = json!(["--speculative-config"]);
            json!(["--speculative-config", speculative()])
        }
        _ => {
            profile["executable"] = json!("/opt/tf/bin/tensorfold");
            profile["build_fingerprint"] = json!("0.6.5");
            profile["security"]["deep_park"] = json!("disabled");
            deployment["residency"] = json!("restart_only");
            deployment["engine_config"] = json!({"context_length": 8192});
            json!([])
        }
    };
    if engine != "tensorfold" {
        let object = deployment.as_object_mut().unwrap();
        object.remove("resources");
        object.remove("residency");
        deployment["engine_config"]["memory"] = json!({"request": "40GiB", "kv_cache": "8GiB"});
        deployment["engine_config"]["accept_extra_args"] = json!(true);
        deployment["engine_config"]["extra_args"] = extra;
    }
    if draft {
        deployment["model"]["draft"] = json!({"http": {
            "url": "https://drafts.example.test/d.tar", "sha256": DIGEST, "archive": "tar"}});
    }
    resolve_effective(&deployment, &host).unwrap()
}

fn sglang_settings(effective: &EffectiveDeployment) -> Value {
    let frozen = frozen_from_effective(
        effective,
        "01K00000000000000000000001",
        "01K00000000000000000000002",
        "127.0.0.1:8123",
        "toy".into(),
        "sglang-inference-binding-1".into(),
        "sglang-admin-binding-1".into(),
    )
    .unwrap();
    SglangLaunch::from_frozen(&frozen)
        .unwrap()
        .public_metadata()["settings"]
        .clone()
}

// T14 T22: SGLang's launch descriptor carries the drafter's directory, which
// the protected entry passes as `speculative_draft_model_path`; a launch
// without a drafter renders exactly as before.
#[test]
fn sglang_is_handed_the_drafters_directory() {
    let settings = sglang_settings(&effective("sglang", true));
    assert_eq!(settings["draft_model_path"], json!(drafter_dir()));
    assert_eq!(
        settings["extra_args"],
        json!(["--speculative-algorithm", "STANDALONE"])
    );
    let settings = sglang_settings(&effective("sglang", false));
    assert!(settings.get("draft_model_path").is_none(), "{settings}");
}

fn vllm_argv(effective: &EffectiveDeployment) -> Vec<String> {
    let plan = vllm::plan_from_effective(effective, 8123, "l".into(), "/r".into()).unwrap();
    vllm::render_command(&plan).unwrap().argv
}

/// The `--speculative-config` values of `argv`, and whether each comes
/// before the extras marker (CapyCTL's rendering) or after it (the extras the
/// entry gates).
fn speculative_configs(argv: &[String]) -> Vec<(Value, bool)> {
    let marker = argv
        .iter()
        .position(|token| token == vllm::args::EXTRA_ARGS_MARKER)
        .unwrap_or(argv.len());
    argv.windows(2)
        .enumerate()
        .filter(|(_, pair)| pair[0] == "--speculative-config")
        .map(|(index, pair)| (serde_json::from_str(&pair[1]).unwrap(), index < marker))
        .collect()
}

// T14 T21: vLLM gets the operator's `--speculative-config` with the
// drafter's directory merged in as `model`, rendered once with the
// host-fixed arguments, outside the extras the entry gates against
// `approved_paths` (the host approved no path).
#[test]
fn vllm_merges_the_drafter_into_the_operators_speculative_config() {
    let argv = vllm_argv(&effective("vllm", true));
    assert_eq!(
        speculative_configs(&argv),
        [(
            json!({"method": "draft_model", "num_speculative_tokens": 4,
                "model": drafter_dir()}),
            true
        )],
        "{argv:?}"
    );
    // Without a drafter the operator's value stays an extra, unchanged.
    let argv = vllm_argv(&effective("vllm", false));
    assert_eq!(
        speculative_configs(&argv),
        [(
            json!({"method": "draft_model", "num_speculative_tokens": 4}),
            false
        )]
    );
}

// T14 T21: the merge takes the one `--speculative-config` from either
// vector, in any spelling, and refuses one that already names a draft
// `model` or states no token count.
#[test]
fn the_vllm_merge_refuses_a_second_draft_path() {
    let args = |tokens: &[&str]| tokens.iter().map(|t| t.to_string()).collect::<Vec<_>>();
    let config = speculative();
    let equals = format!("--speculative-config={config}");
    let (fixed, extra) = vllm_args_with_draft(
        &args(&["--max-num-seqs", "8"]),
        &args(&[&equals, "--seed", "1"]),
        "/d",
    )
    .unwrap();
    assert_eq!(extra, args(&["--seed", "1"]));
    assert_eq!(fixed[..2], args(&["--max-num-seqs", "8"]));
    assert_eq!(fixed[2], "--speculative-config");
    let merged: Value = serde_json::from_str(&fixed[3]).unwrap();
    assert_eq!(merged["model"], "/d");
    // A host-fixed `--speculative-config` takes the drafter where it is.
    let (fixed, extra) =
        vllm_args_with_draft(&args(&["--speculative-config", &config]), &[], "/d").unwrap();
    assert!(extra.is_empty());
    assert_eq!(fixed.len(), 2);
    let named = json!({"method": "draft_model", "model": "/srv/drafters/d",
        "num_speculative_tokens": 4})
    .to_string();
    assert_eq!(
        vllm_args_with_draft(&[], &args(&["--speculative-config", &named]), "/d").unwrap_err(),
        DRAFT_PATH_CONFLICT
    );
    for missing in [
        vec![],
        args(&["--speculative-config", r#"{"method":"draft_model"}"#]),
        args(&["--speculative-config", "not json"]),
    ] {
        assert!(
            vllm_args_with_draft(&[], &missing, "/d").is_err(),
            "{missing:?}"
        );
    }
}

// T41 T14 (ADR 0023 §5): TensorFold is told `--drafter <dir>` in place of
// its default `--drafter none`.
#[test]
fn tensorfold_is_told_the_drafter_instead_of_none() {
    let drafter = |effective: &EffectiveDeployment| {
        let plan = tensorfold::plan_from_effective(effective, 8101, "/var/log/i.log".into(), None)
            .unwrap();
        let argv = tensorfold::render_command(&plan).unwrap().argv;
        argv.windows(2)
            .filter(|pair| pair[0] == "--drafter")
            .map(|pair| pair[1].clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(drafter(&effective("tensorfold", true)), [drafter_dir()]);
    assert_eq!(drafter(&effective("tensorfold", false)), ["none"]);
}
