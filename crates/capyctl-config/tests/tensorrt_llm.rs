//! ADR 0038: the TensorRT-LLM engine kind, its option and key policy and its
//! resolution. CPU tests only; none of this qualifies TensorRT-LLM (only the
//! live rows TR1–TR11, TR1a–TR11a and TR1b–TR11b do).

use capyctl_config::context_fit::{fit_for_launch, ContextSource, StreamsSource};
use capyctl_config::effective::{
    decode_effective_snapshot, resolve_effective, resolve_effective_with_checkpoint,
    CheckpointFacts, EffectiveDeployment, Residency,
};
use capyctl_config::engine_policy::{
    flag_table, validate_extra_args, validate_profile_args, Engine, ExtraArgsContext, FlagClass,
    ProfileArgError,
};
use capyctl_config::lmcache::{engine_gate, LmcacheOffReason};
use capyctl_config::registration::{is_verified, ENVIRONMENT_PROFILES};
use capyctl_config::tensorrt_llm::{RESERVED_NEVER_RENDERED, RESERVED_RENDERED, TYPED_OPTIONS};
use capyctl_config::ConfigErrorCode;
use capyctl_domain::launch::{LaunchSettings, TensorrtLlmLaunchSettings};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::PathBuf;

const GIB: i64 = 1 << 30;

fn fixture() -> (Value, Value) {
    let all: Value = serde_json::from_str(include_str!("fixtures/f2-deployment.json")).unwrap();
    let (mut deployment, mut host) = (all["deployment"].clone(), all["host"].clone());
    let profile = &mut host["runtime_profiles"]["local"];
    profile["engine"] = "tensorrt-llm".into();
    profile["executable"] = "/home/u/tensorrt-llm-1.2.1-venv/bin/trtllm-serve".into();
    profile["build_fingerprint"] = "1.2.1".into();
    profile["args"] = json!([]);
    profile["env"] = json!({});
    profile["security"]["deep_park"] = "disabled".into();
    profile["security"]["approved_paths"] = json!(["/srv/templates"]);
    deployment["residency"] = "restart_only".into();
    deployment.as_object_mut().unwrap().remove("resources");
    deployment["engine_config"] = json!({"memory": {"kv_cache": "4GiB"}});
    (deployment, host)
}

fn measured() -> CheckpointFacts {
    CheckpointFacts {
        weights_bytes: Some(8 * GIB),
        ..CheckpointFacts::default()
    }
}

fn resolve(
    deployment: &Value,
    host: &Value,
) -> Result<EffectiveDeployment, capyctl_config::ConfigError> {
    resolve_effective_with_checkpoint(deployment, host, measured())
}

fn trtllm(effective: &EffectiveDeployment) -> &TensorrtLlmLaunchSettings {
    match &effective.engine_config {
        LaunchSettings::TensorrtLlm(settings) => settings,
        other => panic!("TensorRT-LLM settings, not {other:?}"),
    }
}

fn args(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

fn extra(items: &[&str], approved: &[&str]) -> Result<(), ProfileArgError> {
    let approved_options: BTreeSet<String> = approved
        .iter()
        .map(|name| capyctl_config::engine_policy::normalize_option_name(name))
        .collect();
    let approved_paths = vec![PathBuf::from("/srv/templates")];
    validate_extra_args(
        &args(items),
        &ExtraArgsContext {
            engine: Engine::TensorrtLlm,
            sleep_mode: false,
            approved_options: &approved_options,
            approved_paths: &approved_paths,
            checkpoint_root: None,
            host_fixed: &BTreeSet::new(),
        },
    )
}

// T49 T01 (ADR 0038 §1, owner decision 2026-10-27, question 8): the kind is
// `tensorrt-llm` in every closed set; the command's name is not a kind.
#[test]
fn tensorrt_llm_is_a_named_engine() {
    assert_eq!(Engine::from_name("tensorrt-llm"), Some(Engine::TensorrtLlm));
    assert_eq!(Engine::TensorrtLlm.name(), "tensorrt-llm");
    assert_eq!(
        serde_json::to_value(Engine::TensorrtLlm).unwrap(),
        "tensorrt-llm"
    );
    assert_eq!(
        serde_json::from_value::<Engine>(json!("tensorrt-llm")).unwrap(),
        Engine::TensorrtLlm
    );
    for spelling in ["trtllm", "trtllm-serve", "tensorrt_llm", "tensorrtllm", "TensorRT-LLM"] {
        assert_eq!(Engine::from_name(spelling), None, "{spelling}");
    }
    assert!(Engine::ALL.contains(&Engine::TensorrtLlm));
    assert!(ENVIRONMENT_PROFILES.contains(&"local-tensorrt-llm"));
}

// T49 T17 (ADR 0038 §2, owner rule 2026-10-27): no TensorRT-LLM version is
// in the verified set until its live rows pass; every version lists as
// custom and still resolves.
#[test]
fn no_version_is_verified_before_its_rows_and_none_is_refused() {
    for version in ["1.2.1", "1.3.0rc29", "1.4.0rc1", "1.5.0", "0.21.0"] {
        assert!(!is_verified(Engine::TensorrtLlm, version), "{version}");
        let (deployment, mut host) = fixture();
        host["runtime_profiles"]["local"]["build_fingerprint"] = version.into();
        resolve(&deployment, &host).unwrap_or_else(|e| panic!("{version}: {e}"));
    }
}

// T49 T14 (ADR 0038 §1, §3, owner decision 2026-10-27, question 5): a minimal
// deployment resolves restart-only with 32 requests by default, and its
// snapshot re-resolves to itself.
#[test]
fn a_minimal_deployment_resolves() {
    let (mut deployment, mut host) = fixture();
    let effective = resolve(&deployment, &host).unwrap();
    assert_eq!(effective.residency, Residency::RestartOnly);
    let settings = trtllm(&effective);
    assert_eq!(settings.max_batch_size(), 32);
    assert_eq!(settings.memory.kv_cache_bytes, 4 * GIB);
    assert!(settings.llm_api_options.is_empty());
    let text = serde_json::to_string(&effective).unwrap();
    assert!(text.contains("\"engine\":\"tensorrt-llm\""), "{text}");
    assert_eq!(decode_effective_snapshot(&text).unwrap(), effective);
    // Declared values are kept and restated by the snapshot.
    deployment["engine_config"] = json!({"memory": {"kv_cache": "4GiB"},
        "max_concurrent_requests": 8, "context_length": 16384, "dtype": "bfloat16",
        "kv_cache_dtype": "fp8", "cuda_graphs": false,
        "tensorrt-llm": {"tool_call_parser": "qwen3", "reasoning_parser": "none",
            "llm_api_options": {"enable_chunked_prefill": true,
                "kv_cache_config": {"enable_block_reuse": false}}}});
    let declared = resolve(&deployment, &host).unwrap();
    let settings = trtllm(&declared);
    assert_eq!(settings.max_batch_size(), 8);
    assert_eq!(settings.tool_call_parser.as_deref(), Some("qwen3"));
    assert_eq!(settings.reasoning_parser.as_deref(), Some("none"));
    assert_eq!(settings.llm_api_options["enable_chunked_prefill"], true);
    let text = serde_json::to_string(&declared).unwrap();
    assert_eq!(decode_effective_snapshot(&text).unwrap(), declared);
    assert_ne!(declared.recipe_fingerprint, effective.recipe_fingerprint);
    // A profile that would allow deep parking still defaults to restart_only.
    host["runtime_profiles"]["local"]["security"]["deep_park"] = "enabled".into();
    deployment.as_object_mut().unwrap().remove("residency");
    let defaulted = resolve(&deployment, &host).unwrap();
    assert_eq!(defaulted.residency, Residency::RestartOnly);
}

// T49 (ADR 0038 §1, §3, §11–§13, §16): each refusal before anything starts,
// with its code.
#[test]
fn resolution_refuses_what_tensorrt_llm_cannot_do() {
    let (mut deployment, host) = fixture();
    for residency in ["deep", "host_backed"] {
        deployment["residency"] = residency.into();
        let error = resolve(&deployment, &host).unwrap_err();
        assert_eq!(error.code, ConfigErrorCode::UnsupportedCombination);
        assert!(
            error.detail.contains("capability_missing:deep_park"),
            "{error}"
        );
    }
    deployment["residency"] = "restart_only".into();
    // Decisions (ADR 0030 §6).
    deployment["api"] = "decisions".into();
    let error = resolve(&deployment, &host).unwrap_err();
    assert!(
        error.to_string().contains("capability_missing:decisions"),
        "{error}"
    );
    deployment.as_object_mut().unwrap().remove("api");
    // Several GPUs (§13).
    let devices = deployment.as_object_mut().unwrap().remove("devices");
    deployment["topology"] = json!({"tensor_parallel": 2});
    let error = resolve(&deployment, &host).unwrap_err();
    assert!(error.detail.contains("multi_gpu_unsupported"), "{error}");
    assert!(error.detail.contains("one GPU"), "{error}");
    deployment.as_object_mut().unwrap().remove("topology");
    deployment["devices"] = devices.unwrap();
    // The checkpoint carries its quantization; no language-model-only switch.
    for (field, value) in [
        ("quantization", json!("fp8")),
        ("language_model_only", json!(true)),
        ("kv_cache_dtype", json!("nvfp4")),
    ] {
        deployment["engine_config"] = json!({"memory": {"kv_cache": "4GiB"}});
        deployment["engine_config"][field] = value;
        let error = resolve(&deployment, &host).unwrap_err();
        assert_eq!(error.path, format!("engine_config.{field}"), "{error}");
    }
    // Reserved LLM API keys, nested ones included.
    for options in [
        json!({"enable_sleep": true}),
        json!({"orchestrator_type": "ray"}),
        json!({"kv_connector_config": {}}),
        json!({"kv_cache_config": {"max_tokens": 1}}),
        json!({"kv_cache_config": {"free_gpu_memory_fraction": 0.5}}),
        json!({"env_overrides": {"TLLM_X": "1"}}),
        json!({"speculative_config": {}}),
    ] {
        deployment["engine_config"] = json!({"memory": {"kv_cache": "4GiB"},
            "tensorrt-llm": {"llm_api_options": options}});
        let error = resolve(&deployment, &host).unwrap_err();
        assert_eq!(
            error.path, "engine_config.tensorrt-llm.llm_api_options",
            "{error}"
        );
        assert!(error.detail.contains("reserved"), "{error}");
    }
    // With `cuda_graphs` declared, `cuda_graph_config` is CapyCTL's.
    deployment["engine_config"] = json!({"memory": {"kv_cache": "4GiB"}, "cuda_graphs": true,
        "tensorrt-llm": {"llm_api_options": {"cuda_graph_config": {"max_batch_size": 4}}}});
    assert!(resolve(&deployment, &host).is_err());
    // A served name that is not one path component (owner decision
    // 2026-10-27, question 3).
    deployment["engine_config"] = json!({"memory": {"kv_cache": "4GiB"}});
    for name in ["a/b", "..", "."] {
        deployment["routes"] = json!([name]);
        let error = resolve(&deployment, &host).unwrap_err();
        assert!(error.detail.contains("served_name_unsupported"), "{name}: {error}");
    }
    deployment["routes"] = json!(["toy"]);
    // Another family's block, and TensorRT-LLM's on another family.
    deployment["engine_config"] =
        json!({"memory": {"kv_cache": "4GiB"}, "llamacpp": {"n_gpu_layers": 10}});
    assert_eq!(
        resolve(&deployment, &host).unwrap_err().path,
        "engine_config.llamacpp"
    );
    let all: Value = serde_json::from_str(include_str!("fixtures/f2-deployment.json")).unwrap();
    let mut vllm = all["deployment"].clone();
    vllm["engine_config"]["tensorrt-llm"] = json!({"tool_call_parser": "qwen3"});
    assert_eq!(
        resolve_effective(&vllm, &all["host"]).unwrap_err().path,
        "engine_config.tensorrt-llm"
    );
    // Draft models are out of scope (§1).
    deployment["engine_config"] = json!({"memory": {"kv_cache": "4GiB"}});
    deployment["model"]["draft"] = json!({"type": "local", "path": "drafts/d"});
    assert!(resolve(&deployment, &host).is_err());
}

// T49 T21 (ADR 0038 §3): a profile or deployment environment may not name
// the families and names TensorRT-LLM reads, whatever it approves.
#[test]
fn hidden_inputs_in_the_environment_are_refused() {
    for name in [
        "TLLM_WORKER_USE_SINGLE_PROCESS",
        "TRTLLM_RESPONSES_API_DISABLE_STORE",
        "OMPI_MCA_btl",
        "PMIX_MCA_gds",
        "PRTE_MCA_x",
        "UCX_TLS",
        "MPI4PY_RC_THREADS",
        "HF_HOME",
        "TRANSFORMERS_CACHE",
        "FLASHINFER_CUBIN_DIR",
        "CUDA_VISIBLE_DEVICES",
        "TMPDIR",
        "HOME",
    ] {
        let (deployment, mut host) = fixture();
        host["runtime_profiles"]["local"]["env"] = json!({ name: "1" });
        let error = resolve(&deployment, &host).unwrap_err();
        assert!(error.detail.contains("engine_env_reserved"), "{name}: {error}");
        let (mut deployment, mut host) = fixture();
        host["runtime_profiles"]["local"]["security"]["approved_env"] = json!([name]);
        deployment["engine_config"]["env"] = json!({ name: "1" });
        assert!(resolve(&deployment, &host).is_err(), "{name}");
    }
}

// T49 T21 (ADR 0038 §3, ADR 0014 §6): every reserved option, every alias and
// every `=` spelling is refused; typed spellings name their field; the chat
// template needs approval and an approved path; ordinary options pass.
#[test]
fn every_reserved_spelling_is_refused() {
    for row in RESERVED_RENDERED.iter().chain(RESERVED_NEVER_RENDERED) {
        for spelling in *row {
            for written in [
                vec![(*spelling).to_owned(), "1".to_owned()],
                vec![format!("{spelling}=1")],
            ] {
                let refusal = extra(&written.iter().map(String::as_str).collect::<Vec<_>>(), &[]);
                assert!(
                    matches!(
                        refusal,
                        Err(ProfileArgError::Reserved(_) | ProfileArgError::ReservedField { .. })
                    ),
                    "{written:?}: {refusal:?}"
                );
            }
        }
    }
    for (option, field) in TYPED_OPTIONS {
        match extra(&[option, "1"], &[]) {
            Err(ProfileArgError::ReservedField { field: named, .. }) => assert_eq!(named, *field),
            other => panic!("{option}: {other:?}"),
        }
    }
    // 1.4's sleep switch and the 1.4 `--set` override (§2a).
    assert!(extra(&["--enable_sleep_mode"], &[]).is_err());
    assert!(extra(&["--set", "enable_sleep=true"], &[]).is_err());
    // Subcommands and the positional model.
    assert!(extra(&["disaggregated"], &[]).is_err());
    // Ordinary options pass in both spellings.
    extra(&["--max_num_tokens", "4096", "--enable_chunked_prefill"], &[]).unwrap();
    extra(&["--max_num_tokens=4096"], &[]).unwrap();
    // A duplicate is refused.
    assert!(matches!(
        extra(&["--max_num_tokens", "1", "--max_num_tokens=2"], &[]),
        Err(ProfileArgError::Duplicate(_))
    ));
    // `--chat_template` is sensitive: approval by name, a file in an
    // approved directory.
    assert!(matches!(
        extra(&["--chat_template", "/srv/templates/t.jinja"], &[]),
        Err(ProfileArgError::Sensitive(_))
    ));
    extra(
        &["--chat_template", "/srv/templates/t.jinja"],
        &["--chat_template"],
    )
    .unwrap();
    assert!(matches!(
        extra(&["--chat_template", "/etc/t.jinja"], &["--chat_template"]),
        Err(ProfileArgError::PathNotApproved(_))
    ));
    // Host-fixed arguments follow the same names.
    assert!(validate_profile_args(Engine::TensorrtLlm, &args(&["--tp_size", "2"]), false).is_err());
    validate_profile_args(Engine::TensorrtLlm, &args(&["--max_num_tokens", "8192"]), false)
        .unwrap();
}

// T49 T21 (ADR 0038 §3, ADR 0021): the flag table names every reserved
// spelling as owned and the chat template as sensitive.
#[test]
fn the_flag_table_shows_the_policy() {
    let table = flag_table(Engine::TensorrtLlm);
    for row in RESERVED_RENDERED.iter().chain(RESERVED_NEVER_RENDERED) {
        for spelling in *row {
            assert!(
                table
                    .iter()
                    .any(|r| r.name == *spelling && r.class == FlagClass::Owned),
                "{spelling}"
            );
        }
    }
    assert!(table
        .iter()
        .any(|r| r.name == "--chat_template" && r.class == FlagClass::Sensitive));
}

// T49 T37 (ADR 0038 §12, ADR 0033 §2): LMCache is off with
// `engine_unsupported`.
#[test]
fn lmcache_is_off() {
    let off = engine_gate(Engine::TensorrtLlm, "1.2.1").expect("off");
    assert_eq!(off.reason, LmcacheOffReason::EngineUnsupported);
    let (mut deployment, host) = fixture();
    deployment["lmcache"] = "required".into();
    let error = resolve(&deployment, &host).unwrap_err();
    assert!(error.to_string().contains("engine_unsupported"), "{error}");
}

// T49 (ADR 0038 §3, §4): an undeclared context is fitted to whole KV blocks
// of the grant, capped at the model's window; the batch size is shown as
// the launch's streams.
#[test]
fn the_context_is_fitted_to_whole_blocks() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.json"),
        json!({"model_type": "qwen3", "num_hidden_layers": 36, "num_attention_heads": 32,
            "num_key_value_heads": 8, "head_dim": 128, "torch_dtype": "bfloat16",
            "max_position_embeddings": 40960, "use_sliding_window": false})
        .to_string(),
    )
    .unwrap();
    let (deployment, host) = fixture();
    let effective = resolve(&deployment, &host).unwrap();
    let fit = fit_for_launch(
        Engine::TensorrtLlm,
        &effective.engine_config,
        &[],
        Some(dir.path()),
        None,
    );
    // 4 GiB / 147,456 bytes = 29,127 tokens, 910 whole 32-token blocks.
    assert_eq!(fit.source, ContextSource::Fitted);
    assert_eq!(fit.tokens, Some(29_120));
    let streams = fit.streams.expect("batch size");
    assert_eq!(
        (streams.count, streams.source),
        (Some(32), StreamsSource::Default)
    );
}
