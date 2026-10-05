//! ADR 0014 §5 (owner decision 2026-09-25): an undeclared context is fitted to
//! the KV cache grant at launch render, on both engines, from the checkpoint's
//! `config.json`. The shared builders run where the checkpoint is (the
//! embedded host, or the host agent), so both paths pass the same value.
//! CPU tests only: nothing here qualifies a native engine recipe.

use capyctl_adapters::sglang::frozen_from_effective;
use capyctl_adapters::vllm::{plan_from_effective, render_command};
use capyctl_config::effective::{resolve_effective, EffectiveDeployment};
use serde_json::{json, Value};

/// A dense model at 512 KiB of bfloat16 KV per token: 4 GiB holds 8192.
fn dense() -> Value {
    json!({
        "num_hidden_layers": 32, "num_attention_heads": 32, "hidden_size": 4096,
        "max_position_embeddings": 32768, "torch_dtype": "bfloat16",
    })
}

fn golden(name: &str) -> (Value, Value) {
    let text = match name {
        "vllm" => include_str!("../../capyctl-config/tests/fixtures/effective-vllm-golden.json"),
        _ => include_str!("../../capyctl-config/tests/fixtures/effective-sglang-golden.json"),
    };
    let source: Value = serde_json::from_str(text).unwrap();
    (
        source["input"]["deployment"].clone(),
        source["input"]["host"].clone(),
    )
}

/// The golden deployment against a real checkpoint directory holding `config`.
fn resolved(
    engine: &str,
    config: Option<&Value>,
    edit: impl FnOnce(&mut Value, &mut Value),
) -> (tempfile::TempDir, EffectiveDeployment) {
    let store = tempfile::tempdir().unwrap();
    let checkpoint = store.path().join("toy");
    std::fs::create_dir(&checkpoint).unwrap();
    if let Some(config) = config {
        std::fs::write(checkpoint.join("config.json"), config.to_string()).unwrap();
    }
    let (mut deployment, mut host) = golden(engine);
    host["model_store"]["path"] = json!(store.path());
    deployment["model"]["path"] = json!(checkpoint);
    deployment["engine_config"]["memory"]["kv_cache"] = json!("4GiB");
    if engine == "vllm" {
        // No host-fixed `--max-model-len`, as the environment profile now is.
        host["runtime_profiles"]["local"]["args"] = json!([]);
    }
    edit(&mut deployment, &mut host);
    let effective = resolve_effective(&deployment, &host).unwrap();
    (store, effective)
}

fn vllm_argv(effective: &EffectiveDeployment) -> Vec<String> {
    let plan = plan_from_effective(effective, 8123, "l".into(), "/r".into()).unwrap();
    render_command(&plan).unwrap().argv
}

fn max_model_len(argv: &[String]) -> Option<&str> {
    argv.windows(2)
        .find(|w| w[0] == "--max-model-len")
        .map(|w| w[1].as_str())
}

fn sglang_context(effective: &EffectiveDeployment) -> Option<u32> {
    let frozen = frozen_from_effective(
        effective,
        "binding-1",
        "01K00000000000000000000002",
        "127.0.0.1:8123",
        "toy".into(),
        "sglang-inference-binding-1".into(),
        "sglang-admin-binding-1".into(),
    )
    .unwrap();
    frozen.settings().common.context_length
}

// T14: the fitted value reaches vLLM's `--max-model-len`, one 16-token
// block short of the grant (vLLM keeps a null block).
#[test]
fn vllm_renders_the_context_fitted_to_the_grant() {
    let (_store, effective) = resolved("vllm", Some(&dense()), |_, _| {});
    assert_eq!(max_model_len(&vllm_argv(&effective)), Some("8176"));
    // An fp8 KV cache holds twice the tokens.
    let (_store, effective) = resolved("vllm", Some(&dense()), |d, _| {
        d["engine_config"]["kv_cache_dtype"] = json!("fp8");
    });
    assert_eq!(max_model_len(&vllm_argv(&effective)), Some("16368"));
}

// T14: the fitted value reaches SGLang's `context_length` setting, which the
// protected entry renders as `--context-length`.
#[test]
fn sglang_carries_the_context_fitted_to_the_grant() {
    let (_store, effective) = resolved("sglang", Some(&dense()), |_, _| {});
    assert_eq!(sglang_context(&effective), Some(8192));
    let (_store, effective) = resolved("sglang", Some(&dense()), |d, _| {
        d["engine_config"]["memory"]["kv_cache"] = json!("64GiB");
        d["engine_config"]["memory"]["request"] = json!("70GiB");
        d.as_object_mut().unwrap().remove("resources");
    });
    // Capped at the model's maximum position.
    assert_eq!(sglang_context(&effective), Some(32768));
}

// T14: an explicit context_length always wins, on both engines.
#[test]
fn an_explicit_context_wins_on_both_engines() {
    for engine in ["vllm", "sglang"] {
        let (_store, effective) = resolved(engine, Some(&dense()), |d, _| {
            d["engine_config"]["context_length"] = json!(2048);
        });
        match engine {
            "vllm" => assert_eq!(max_model_len(&vllm_argv(&effective)), Some("2048")),
            _ => assert_eq!(sglang_context(&effective), Some(2048)),
        }
    }
}

// T14: a checkpoint whose shape cannot be read falls back to 4096.
#[test]
fn an_unreadable_shape_falls_back_on_both_engines() {
    let (_store, effective) = resolved("vllm", None, |_, _| {});
    assert_eq!(max_model_len(&vllm_argv(&effective)), Some("4096"));
    let fit = capyctl_config::context_fit::fit_for_effective(&effective);
    assert!(fit.reason.unwrap().contains("config.json"));
    let (_store, effective) = resolved("sglang", Some(&json!({"kv_lora_rank": 512})), |_, _| {});
    assert_eq!(sglang_context(&effective), Some(4096));
}

// T14: a host-fixed `--max-model-len` (an explicit CAPYCTL_ENGINE_ARGS) is kept
// and capyctl passes no second one.
#[test]
fn a_host_fixed_context_is_kept() {
    let (_store, effective) = resolved("vllm", Some(&dense()), |_, host| {
        host["runtime_profiles"]["local"]["args"] = json!(["--max-model-len", "4096"]);
    });
    let argv = vllm_argv(&effective);
    assert_eq!(
        argv.iter().filter(|a| *a == "--max-model-len").count(),
        1,
        "{argv:?}"
    );
    assert_eq!(max_model_len(&argv), Some("4096"));
    let fit = capyctl_config::context_fit::fit_for_effective(&effective);
    assert_eq!(
        fit.source,
        capyctl_config::context_fit::ContextSource::HostFixed
    );
}

// T14 (found live 2026-10-02): with a draft model, the fitted context counts
// the draft model's KV layers too, on both engines: vLLM 0.30 with DFlash2
// failed to start at the context fitted to the checkpoint's layers alone.
#[test]
fn a_draft_models_kv_layers_shorten_the_fitted_context() {
    let drafters = tempfile::tempdir().unwrap();
    let draft = drafters.path().join("d");
    std::fs::create_dir(&draft).unwrap();
    // 2 × 4 × 8 × 128 × 2 = 16 KiB of bfloat16 KV per token.
    let draft_config = json!({
        "num_hidden_layers": 4, "num_attention_heads": 32, "num_key_value_heads": 8,
        "head_dim": 128, "max_position_embeddings": 32768, "torch_dtype": "bfloat16",
    });
    std::fs::write(draft.join("config.json"), draft_config.to_string()).unwrap();
    let raw = (4u64 << 30) / (512 * 1024 + 16 * 1024);
    let fitted = raw - raw % 16;
    for engine in ["vllm", "sglang"] {
        let (_store, effective) = resolved(engine, Some(&dense()), |d, host| {
            let profile = &mut host["runtime_profiles"]["local"];
            profile["security"]["approved_options"] =
                json!(["--speculative-config", "--speculative-draft-model-path"]);
            profile["security"]["approved_paths"] = json!([drafters.path()]);
            d["engine_config"]["accept_extra_args"] = json!(true);
            d["engine_config"]["extra_args"] = match engine {
                "vllm" => json!([
                    "--speculative-config",
                    json!({"method": "draft_model", "model": draft, "num_speculative_tokens": 3})
                        .to_string()
                ]),
                _ => json!(["--speculative-draft-model-path", draft]),
            };
        });
        match engine {
            // vLLM keeps one 16-token block.
            "vllm" => assert_eq!(
                max_model_len(&vllm_argv(&effective)),
                Some((fitted - 16).to_string().as_str())
            ),
            _ => assert_eq!(sglang_context(&effective), Some(fitted as u32)),
        }
    }
}

fn max_num_seqs(argv: &[String]) -> Option<&str> {
    argv.windows(2)
        .find(|w| w[0] == "--max-num-seqs")
        .map(|w| w[1].as_str())
}

// T14 (owner decision 2026-10-02): vLLM runs as many sequences as CapyCTL
// keeps in flight per deployment unless the deployment or the installation
// sets its own.
#[test]
fn vllm_runs_the_routers_in_flight_bound_unless_told_otherwise() {
    let bound = capyctl_domain::launch::MAX_REQUESTS_PER_DEPLOYMENT.to_string();
    let (_store, effective) = resolved("vllm", Some(&dense()), |_, _| {});
    assert_eq!(max_num_seqs(&vllm_argv(&effective)), Some(bound.as_str()));
    let (_store, effective) = resolved("vllm", Some(&dense()), |d, _| {
        d["engine_config"]["max_concurrent_requests"] = json!(8);
    });
    assert_eq!(max_num_seqs(&vllm_argv(&effective)), Some("8"));
    let (_store, effective) = resolved("vllm", Some(&dense()), |_, host| {
        host["runtime_profiles"]["local"]["args"] = json!(["--max-num-seqs", "64"]);
    });
    let argv = vllm_argv(&effective);
    assert_eq!(
        argv.iter().filter(|a| *a == "--max-num-seqs").count(),
        1,
        "{argv:?}"
    );
    assert_eq!(max_num_seqs(&argv), Some("64"));
}

/// Qwen3.8-27B NVFP4's language model: 48 gated-delta-net layers and 16
/// attention layers with 4 KV heads of 256.
fn qwen38_27b() -> Value {
    let mut types = Vec::new();
    for _ in 0..16 {
        types.extend(["linear_attention", "linear_attention", "linear_attention"]);
        types.push("full_attention");
    }
    json!({"model_type": "qwen3_5", "text_config": {
        "model_type": "qwen3_5_text", "num_hidden_layers": 64, "num_attention_heads": 24,
        "num_key_value_heads": 4, "head_dim": 256, "hidden_size": 5120,
        "max_position_embeddings": 262144, "dtype": "bfloat16", "layer_types": types,
        "full_attention_interval": 4, "linear_conv_kernel_dim": 4, "linear_key_head_dim": 128,
        "linear_num_key_heads": 16, "linear_num_value_heads": 48, "linear_value_head_dim": 128,
        "mamba_ssm_dtype": "float32",
    }})
}

fn sglang_frozen(
    effective: &EffectiveDeployment,
) -> Result<capyctl_domain::launch::NativeLaunch, capyctl_adapters::RuntimeError> {
    frozen_from_effective(
        effective,
        "01K00000000000000000000001",
        "01K00000000000000000000002",
        "127.0.0.1:8123",
        "toy".into(),
        "sglang-inference-binding-1".into(),
        "sglang-admin-binding-1".into(),
    )
}

/// The settings the protected entry is given.
fn sglang_public(effective: &EffectiveDeployment) -> Value {
    let frozen = sglang_frozen(effective).unwrap();
    let launch = capyctl_adapters::sglang::SglangLaunch::from_frozen(&frozen).unwrap();
    launch.public_metadata()["settings"].clone()
}

/// The golden SGLang deployment with `memory`, sized with `weights` of
/// checkpoint weights.
fn sized_sglang(
    config: &Value,
    weights: Option<i64>,
    edit: impl FnOnce(&mut Value),
) -> (tempfile::TempDir, EffectiveDeployment) {
    let store = tempfile::tempdir().unwrap();
    let checkpoint = store.path().join("toy");
    std::fs::create_dir(&checkpoint).unwrap();
    std::fs::write(checkpoint.join("config.json"), config.to_string()).unwrap();
    let (mut deployment, mut host) = golden("sglang");
    host["model_store"]["path"] = json!(store.path());
    deployment["model"]["path"] = json!(checkpoint);
    deployment.as_object_mut().unwrap().remove("resources");
    deployment["engine_config"]["kv_cache_dtype"] = json!("fp8_e4m3");
    edit(&mut deployment);
    let facts = capyctl_config::effective::CheckpointFacts {
        weights_bytes: weights,
        ..Default::default()
    };
    let effective =
        capyctl_config::effective::resolve_effective_with_checkpoint(&deployment, &host, facts)
            .unwrap();
    (store, effective)
}

// T14 (owner decision 2026-10-03, ADR 0014 amendment A14): SGLang is given
// the KV cache as its KV pool in tokens, as vLLM is given it in bytes.
#[test]
fn sglang_is_given_the_kv_cache_as_its_kv_pool() {
    let (_store, effective) = resolved("sglang", Some(&dense()), |_, _| {});
    let settings = sglang_public(&effective);
    // 4 GiB / 512 KiB of bfloat16 KV per token.
    assert_eq!(settings["max_total_tokens"], json!(8192));
    assert_eq!(settings["max_running_requests"], Value::Null);
    assert!(settings.get("max_mamba_cache_size").is_none(), "{settings}");
}

// T14 (amendment A14, found live 2026-10-03): a hybrid model's KV pool counts
// its attention layers only, and its recurrent state is sized for its running
// requests beside the KV cache instead of taking a share of it.
#[test]
fn sglang_sizes_a_hybrid_models_state_beside_its_kv_cache() {
    let weights = 21_920_000_000;
    let (_store, effective) = sized_sglang(&qwen38_27b(), Some(weights), |d| {
        d["engine_config"]["memory"] = json!({"request": "96GiB", "kv_cache": "16GiB"});
    });
    let settings = sglang_public(&effective);
    assert_eq!(settings["max_total_tokens"], json!(524288));
    assert_eq!(
        settings["max_running_requests"],
        json!(capyctl_domain::launch::MAX_REQUESTS_PER_DEPLOYMENT)
    );
    assert_eq!(settings["max_mamba_cache_size"], json!(160));
    // A declared count is kept, and its state sized.
    let (_store, effective) = sized_sglang(&qwen38_27b(), Some(weights), |d| {
        d["engine_config"]["memory"] = json!({"request": "96GiB", "kv_cache": "16GiB"});
        d["engine_config"]["max_concurrent_requests"] = json!(8);
    });
    let settings = sglang_public(&effective);
    assert_eq!(settings["max_running_requests"], json!(8));
    assert_eq!(settings["max_mamba_cache_size"], json!(40));
}

// T14 (amendment A14): a state that does not fit the memory request is
// refused before anything starts, naming the request that would hold it, and
// status shows the same reason beforehand.
#[test]
fn sglang_refuses_a_hybrid_state_the_request_cannot_hold() {
    let (_store, effective) = sized_sglang(&qwen38_27b(), Some(21_920_000_000), |d| {
        d["engine_config"]["memory"] = json!({"request": "40GiB", "kv_cache": "16GiB"});
        d["engine_config"]["max_concurrent_requests"] = json!(8);
    });
    let Err(error) = sglang_frozen(&effective) else {
        panic!("refused")
    };
    let error = error.to_string();
    assert!(
        error.contains("engine_config.memory.request") && error.contains("max_concurrent_requests"),
        "{error}"
    );
    let fit = capyctl_config::context_fit::fit_for_effective(&effective);
    assert!(fit
        .warning
        .is_some_and(|warning| warning.contains("engine_config.memory.request")),);
}

// T14 (#40's rule): extra arguments that size the state pool win, and
// CapyCTL passes nothing for it.
#[test]
fn extra_arguments_that_size_sglangs_state_pool_win() {
    let (_store, effective) = sized_sglang(&qwen38_27b(), Some(21_920_000_000), |d| {
        d["engine_config"]["memory"] = json!({"request": "96GiB", "kv_cache": "16GiB"});
        d["engine_config"]["accept_extra_args"] = json!(true);
        d["engine_config"]["extra_args"] = json!(["--max-mamba-cache-size", "64"]);
    });
    let settings = sglang_public(&effective);
    assert!(settings.get("max_mamba_cache_size").is_none(), "{settings}");
    assert_eq!(settings["max_running_requests"], Value::Null);
    assert_eq!(settings["max_total_tokens"], json!(524288));
}

// T14 (owner decision 2026-10-03): a derived request fits what it can: the
// state takes part of the margin, SGLang's static pool grows by it, and the
// running requests are limited to what fits.
#[test]
fn a_derived_request_lends_the_state_part_of_its_margin() {
    let weights = 21_920_000_000i64;
    let (_store, effective) = sized_sglang(&qwen38_27b(), Some(weights), |d| {
        d["engine_config"]["memory"] = json!({"kv_cache": "4GiB"});
    });
    let settings = sglang_public(&effective);
    let state = 153_944_064i64;
    assert_eq!(settings["max_running_requests"], json!(5));
    assert_eq!(settings["max_mamba_cache_size"], json!(25));
    let gib = 1i64 << 30;
    assert_eq!(
        settings["memory"]["static_bytes"],
        json!(weights + 4 * gib + 26 * state + 2 * gib)
    );
    let fit = capyctl_config::context_fit::fit_for_effective(&effective);
    assert_eq!(fit.running_limit, Some(5));
}

// T14 (amendment A14, found live 2026-10-03): a revision that declares its
// request and KV cache records no weights, so the launch sizes the weight
// files it reads and still refuses a state that does not fit.
#[test]
fn sglang_sizes_the_weights_at_launch_when_the_revision_has_none() {
    let (store, effective) = sized_sglang(&qwen38_27b(), None, |d| {
        d["engine_config"]["memory"] = json!({"request": "40GiB", "kv_cache": "16GiB"});
        d["engine_config"]["max_concurrent_requests"] = json!(8);
    });
    // A sparse file stands for the weights.
    std::fs::File::create(store.path().join("toy/model.safetensors"))
        .unwrap()
        .set_len(21_920_000_000)
        .unwrap();
    let Err(error) = sglang_frozen(&effective) else {
        panic!("refused")
    };
    let error = error.to_string();
    assert!(error.contains("21920000000 bytes of weights"), "{error}");
}

/// FrogNano-4B-2609's language model: 24 gated-delta-net layers and 8
/// attention layers of 4 KV heads of 256; 51511296 bytes of state a request.
fn frognano_4b() -> Value {
    let mut types = Vec::new();
    for _ in 0..8 {
        types.extend(["linear_attention", "linear_attention", "linear_attention"]);
        types.push("full_attention");
    }
    json!({"model_type": "qwen3_5", "text_config": {
        "model_type": "qwen3_5_text", "num_hidden_layers": 32, "num_attention_heads": 16,
        "num_key_value_heads": 4, "head_dim": 256, "hidden_size": 2560,
        "max_position_embeddings": 262144, "dtype": "bfloat16", "layer_types": types,
        "full_attention_interval": 4, "linear_conv_kernel_dim": 4, "linear_key_head_dim": 128,
        "linear_num_key_heads": 16, "linear_num_value_heads": 32, "linear_value_head_dim": 128,
        "mamba_ssm_dtype": "float32",
    }})
}

/// The golden SGLang deployment, stating no memory, on a 16 GB laptop GPU
/// (16376 MiB, the standalone host's device domain: 8 % reserved).
fn laptop_sglang(
    config: &Value,
    weights: i64,
    edit: impl FnOnce(&mut Value),
) -> (tempfile::TempDir, EffectiveDeployment) {
    let store = tempfile::tempdir().unwrap();
    let checkpoint = store.path().join("toy");
    std::fs::create_dir(&checkpoint).unwrap();
    std::fs::write(checkpoint.join("config.json"), config.to_string()).unwrap();
    let (mut deployment, mut host) = golden("sglang");
    host["model_store"]["path"] = json!(store.path());
    host["resource_policy"]["domains"] = json!({
        "system": {"memory": "distinct", "managed_limit": "30GiB", "free_reserve": "12GiB",
                   "parked_limit": "15GiB"},
        "gpu0": {"memory": "device", "device": "gpu0", "managed_limit": "15797762136B",
                 "free_reserve": "1373718440B", "parked_limit": "4292870144B"}
    });
    host["resource_policy"]["devices"] = json!({"gpu0": {"domain": "gpu0", "sharing": "shared"}});
    deployment["model"]["path"] = json!(checkpoint);
    deployment.as_object_mut().unwrap().remove("resources");
    deployment["engine_config"]
        .as_object_mut()
        .unwrap()
        .remove("memory");
    edit(&mut deployment);
    let facts = capyctl_config::effective::CheckpointFacts {
        weights_bytes: Some(weights),
        ..Default::default()
    };
    let effective =
        capyctl_config::effective::resolve_effective_with_checkpoint(&deployment, &host, facts)
            .unwrap();
    (store, effective)
}

fn named_mib(text: &str) -> String {
    text.split("memory.request to \"")
        .nth(1)
        .and_then(|tail| tail.split('"').next())
        .unwrap_or_else(|| panic!("no named request: {text}"))
        .to_owned()
}

// Found live 2026-10-04 (FrogNano-4B BF16, SGLang 0.5.21, a 16 GB laptop
// GPU): a deployment stating no memory was refused for one running request.
// Its derived request fits what it can: the state takes up to half of the KV
// cache CapyCTL chose, the KV pool and the fitted context hold the rest, and
// status says so before the start, as the start does.
#[test]
fn a_derived_hybrid_on_a_discrete_gpu_fits_what_it_can() {
    let weights = 9_319_820_920i64;
    let (_store, effective) = laptop_sglang(&frognano_4b(), weights, |_| {});
    let kv = effective.engine_config.memory().kv_cache_bytes;
    assert_eq!(kv, 15_797_762_136 / 4);
    let state = 36 * 51_511_296i64;
    let pool_tokens = (kv - state) / 32768;
    let fit = capyctl_config::context_fit::fit_for_effective(&effective);
    assert_eq!(fit.warning, None);
    assert_eq!(fit.running_limit, Some(7));
    assert_eq!(fit.tokens, Some((pool_tokens - pool_tokens % 16) as u32));
    let sized = effective
        .with_device_total(|index| (index == 0).then_some(16376 << 20))
        .unwrap();
    let settings = sglang_public(&sized);
    assert_eq!(settings["max_running_requests"], json!(7));
    assert_eq!(settings["max_mamba_cache_size"], json!(35));
    assert_eq!(settings["max_total_tokens"], json!(pool_tokens));
    assert_eq!(settings["context_length"], json!(fit.tokens));
}

// Found live 2026-10-04: status sized a discrete deployment's static pool as
// unified memory's, so it named another request than the refused start.
#[test]
fn status_and_a_refused_discrete_start_name_the_same_request() {
    let (_store, effective) = laptop_sglang(&frognano_4b(), 9_319_820_920, |d| {
        d["engine_config"]["memory"] = json!({"request": "12GiB", "kv_cache": "3GiB"});
    });
    let warning = capyctl_config::context_fit::fit_for_effective(&effective)
        .warning
        .expect("status warns");
    let sized = effective
        .with_device_total(|index| (index == 0).then_some(16376 << 20))
        .unwrap();
    let Err(error) = sglang_frozen(&sized) else {
        panic!("refused")
    };
    assert_eq!(named_mib(&warning), named_mib(&error.to_string()));
}

// ADR 0014, note on amendment A14 (found live 2026-10-04, FrogNano-4B BF16,
// SGLang 0.5.21, a 16 GB laptop GPU): SGLang took its fraction of the 15.24
// GiB free when it started, not of the card's 15.99 GiB, and held 42778 of the
// 63935 KV tokens passed. The fraction for a discrete launch whose pools are
// fixed now carries the baseline allowance; unified memory renders as before.
#[test]
fn a_discrete_sglang_fraction_carries_the_baseline_allowance() {
    let weights = 9_319_820_920i64;
    let (_store, effective) = laptop_sglang(&frognano_4b(), weights, |_| {});
    let sized = effective
        .with_device_total(|index| (index == 0).then_some(16376 << 20))
        .unwrap();
    let memory = sized.engine_config.memory();
    let static_pool = memory.request_bytes - (memory.request_bytes - memory.kv_cache_bytes) / 11;
    let settings = sglang_public(&sized);
    assert_eq!(settings["memory"]["static_bytes"], json!(static_pool));
    assert_eq!(
        settings["memory"]["static_allowance_bytes"],
        json!(capyctl_config::context_fit::DISCRETE_BASELINE_ALLOWANCE_BYTES)
    );
    let weights = 21_920_000_000;
    let (_store, effective) = sized_sglang(&qwen38_27b(), Some(weights), |d| {
        d["engine_config"]["memory"] = json!({"request": "96GiB", "kv_cache": "16GiB"});
    });
    let settings = sglang_public(&effective);
    assert_eq!(settings["memory"]["static_bytes"], json!(88i64 << 30));
    assert!(settings["memory"].get("static_allowance_bytes").is_none());
}
