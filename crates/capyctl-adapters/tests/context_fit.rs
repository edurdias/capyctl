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
