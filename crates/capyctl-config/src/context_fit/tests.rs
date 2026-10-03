use super::*;
use serde_json::json;

const GIB: i64 = 1 << 30;

/// A dense multi-head model: 32 layers, 32 KV heads of 128 (hidden 4096),
/// bfloat16, 32768 positions. 2 × 32 × 32 × 128 × 2 = 512 KiB per token.
fn dense() -> Value {
    json!({
        "model_type": "llama", "num_hidden_layers": 32, "num_attention_heads": 32,
        "hidden_size": 4096, "max_position_embeddings": 32768, "torch_dtype": "bfloat16",
    })
}

fn inputs(kv: i64) -> FitInputs<'static> {
    FitInputs {
        declared: None,
        kv_cache_bytes: kv,
        kv_cache_dtype: None,
        dtype: None,
        block_tokens: None,
        reserved_blocks: 0,
        draft: None,
        vllm: None,
        sglang: false,
    }
}

// T14: the effective context is derived with provenance, not left to the
// engine's own default.
#[test]
fn a_dense_model_fits_the_grant_to_whole_blocks() {
    // 4 GiB / 512 KiB = 8192 tokens exactly.
    let fit = fit_context(inputs(4 * GIB), Ok(&dense()));
    assert_eq!(fit.tokens, Some(8192));
    assert_eq!(fit.source, ContextSource::Fitted);
    assert_eq!(fit.reason, None);
    // A grant that is not a whole number of blocks rounds down.
    let fit = fit_context(inputs(4 * GIB + 3 * 512 * 1024), Ok(&dense()));
    assert_eq!(fit.tokens, Some(8192));
    let mut block = inputs(4 * GIB + 40 * 512 * 1024);
    assert_eq!(fit_context(block, Ok(&dense())).tokens, Some(8192 + 32));
    block.block_tokens = Some(64);
    assert_eq!(fit_context(block, Ok(&dense())).tokens, Some(8192));
}

// T14
#[test]
fn grouped_query_attention_counts_only_the_kv_heads() {
    // 28 layers, 4 KV heads of 128 (explicit head_dim), bfloat16:
    // 2 × 28 × 4 × 128 × 2 = 57344 bytes per token.
    let config = json!({
        "num_hidden_layers": 28, "num_attention_heads": 28, "num_key_value_heads": 4,
        "head_dim": 128, "hidden_size": 3584, "max_position_embeddings": 131072,
        "torch_dtype": "bfloat16",
    });
    let fit = fit_context(inputs(2 * GIB), Ok(&config));
    let raw = (2 * GIB) as u64 / 57344;
    assert_eq!(fit.tokens, Some((raw - raw % 16) as u32));
    assert_eq!(fit.source, ContextSource::Fitted);
}

// T14
#[test]
fn an_fp8_kv_cache_holds_twice_the_tokens() {
    let mut fp8 = inputs(2 * GIB);
    fp8.kv_cache_dtype = Some("fp8_e4m3");
    assert_eq!(fit_context(fp8, Ok(&dense())).tokens, Some(8192));
    // `auto` follows the deployment's dtype, then the checkpoint's.
    let mut auto = inputs(2 * GIB);
    auto.kv_cache_dtype = Some("auto");
    assert_eq!(fit_context(auto, Ok(&dense())).tokens, Some(4096));
    auto.dtype = Some("float32");
    assert_eq!(fit_context(auto, Ok(&dense())).tokens, Some(2048));
}

// T14
#[test]
fn the_context_is_capped_at_the_models_maximum_position() {
    let fit = fit_context(inputs(64 * GIB), Ok(&dense()));
    assert_eq!(fit.tokens, Some(32768));
    assert_eq!(fit.source, ContextSource::Fitted);
}

// T14: a shape that cannot be computed reliably falls back to 4096 and says why.
#[test]
fn an_uncomputable_shape_falls_back_with_its_reason() {
    let mla = json!({
        "num_hidden_layers": 61, "num_attention_heads": 128, "kv_lora_rank": 512,
        "hidden_size": 7168, "max_position_embeddings": 163840, "torch_dtype": "bfloat16",
    });
    let mut no_dtype = dense();
    no_dtype.as_object_mut().unwrap().remove("torch_dtype");
    let mut no_positions = dense();
    no_positions
        .as_object_mut()
        .unwrap()
        .remove("max_position_embeddings");
    for (config, reason) in [
        (Ok(mla), "latent attention"),
        (Ok(no_dtype), "names no dtype"),
        (Ok(no_positions), "max_position_embeddings"),
        (Ok(json!({})), "num_hidden_layers"),
        (
            Err("the checkpoint has no readable config.json".into()),
            "config.json",
        ),
    ] {
        let fit = fit_context(inputs(4 * GIB), config.as_ref().map_err(Clone::clone));
        assert_eq!(fit.tokens, Some(FALLBACK_CONTEXT), "{reason}");
        assert_eq!(fit.source, ContextSource::Fallback);
        assert!(fit.reason.as_deref().unwrap().contains(reason), "{fit:?}");
    }
    // An unknown KV dtype is not guessed.
    let mut unknown = inputs(4 * GIB);
    unknown.kv_cache_dtype = Some("int3");
    assert_eq!(
        fit_context(unknown, Ok(&dense())).source,
        ContextSource::Fallback
    );
    // A grant smaller than one block cannot be fitted either.
    let tiny = fit_context(inputs(1024), Ok(&dense()));
    assert_eq!(tiny.source, ContextSource::Fallback);
}

// T14: sliding-window and hybrid layers are counted as full attention.
#[test]
fn sliding_window_and_hybrid_models_are_counted_conservatively() {
    let mut sliding = dense();
    sliding["sliding_window"] = json!(4096);
    let fit = fit_context(inputs(4 * GIB), Ok(&sliding));
    assert_eq!(fit.tokens, Some(8192));
    assert!(fit.reason.unwrap().contains("sliding-window"));
    // A disabled window is plain full attention.
    sliding["use_sliding_window"] = json!(false);
    assert_eq!(fit_context(inputs(4 * GIB), Ok(&sliding)).reason, None);
    let mut hybrid = dense();
    hybrid["layer_types"] = json!(["linear_attention", "full_attention"]);
    let fit = fit_context(inputs(4 * GIB), Ok(&hybrid));
    assert_eq!(fit.tokens, Some(8192));
    assert!(fit.reason.unwrap().contains("hybrid"));
    // A multimodal checkpoint keeps the language model under text_config.
    let multimodal = json!({"model_type": "vl", "text_config": dense()});
    assert_eq!(
        fit_context(inputs(4 * GIB), Ok(&multimodal)).tokens,
        Some(8192)
    );
}

// T14: an explicit context always wins; one the grant provably cannot hold
// carries a warning, and the launch is left to the engine as before.
#[test]
fn an_explicit_context_wins_and_warns_when_it_cannot_fit() {
    let mut declared = inputs(4 * GIB);
    declared.declared = Some(4096);
    let fit = fit_context(declared, Ok(&dense()));
    assert_eq!(
        (fit.tokens, fit.source, fit.warning),
        (Some(4096), ContextSource::Declared, None)
    );
    declared.declared = Some(16384);
    let fit = fit_context(declared, Ok(&dense()));
    assert_eq!(fit.tokens, Some(16384));
    assert_eq!(fit.source, ContextSource::Declared);
    let warning = fit.warning.unwrap();
    assert!(
        warning.contains("16384") && warning.contains("8192"),
        "{warning}"
    );
    // An approximate shape never warns: it overstates the KV per token.
    let mut sliding = dense();
    sliding["sliding_window"] = json!(4096);
    assert_eq!(fit_context(declared, Ok(&sliding)).warning, None);
    // No configuration: the declared value is used unchanged.
    let fit = fit_context(declared, Err("missing".into()));
    assert_eq!((fit.tokens, fit.warning), (Some(16384), None));
}

// T14: the model configuration is read from the checkpoint directory.
#[test]
fn the_configuration_is_read_from_the_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    assert!(read_model_config(dir.path()).is_err());
    std::fs::write(dir.path().join("config.json"), dense().to_string()).unwrap();
    assert_eq!(read_model_config(dir.path()).unwrap(), dense());
    std::fs::write(dir.path().join("config.json"), "not json").unwrap();
    assert!(read_model_config(dir.path()).is_err());
}

// T14: vLLM keeps part of its KV pool for itself (a null block, and its pool
// rounding), so a context fitted to the exact grant is refused at start.
// Found live on the discrete-GPU laptop host: Qwen3-4B (36 layers, 8 KV heads
// of 128, bfloat16) with a 3949440534-byte grant was fitted to 26768 tokens,
// and vLLM 0.29 refused it ("estimated maximum model length is 26752").
#[test]
fn a_vllm_fit_leaves_the_engines_reserved_blocks() {
    let qwen3_4b = json!({
        "num_hidden_layers": 36, "num_attention_heads": 32, "num_key_value_heads": 8,
        "head_dim": 128, "hidden_size": 2560, "max_position_embeddings": 262144,
        "torch_dtype": "bfloat16",
    });
    let mut vllm = inputs(3_949_440_534);
    assert_eq!(fit_context(vllm, Ok(&qwen3_4b)).tokens, Some(26768));
    vllm.reserved_blocks = VLLM_RESERVED_BLOCKS;
    let fit = fit_context(vllm, Ok(&qwen3_4b));
    assert_eq!(fit.tokens, Some(26752));
    assert_eq!(fit.source, ContextSource::Fitted);
    // The model's own maximum still caps a large grant unchanged.
    let mut large = inputs(64 * GIB);
    large.reserved_blocks = VLLM_RESERVED_BLOCKS;
    assert_eq!(fit_context(large, Ok(&dense())).tokens, Some(32768));
    // A grant of no more than the reserved blocks cannot be fitted.
    let mut tiny = inputs(16 * 512 * 1024 + 1000);
    tiny.reserved_blocks = VLLM_RESERVED_BLOCKS;
    assert_eq!(
        fit_context(tiny, Ok(&dense())).source,
        ContextSource::Fallback
    );
}

/// The DFlash2 draft model of Qwen3.8-27B as its `config.json` states it:
/// five sliding-window layers, 8 KV heads of 128, bfloat16. 2 × 5 × 8 × 128
/// = 10240 elements per token.
fn dflash_drafter() -> Value {
    json!({
        "architectures": ["DFlash2DraftModel"], "model_type": "qwen3",
        "num_hidden_layers": 5, "num_attention_heads": 32, "num_key_value_heads": 8,
        "head_dim": 128, "hidden_size": 5120, "max_position_embeddings": 262144,
        "dtype": "bfloat16", "sliding_window": 2048, "use_sliding_window": true,
        "layer_types": ["sliding_attention", "sliding_attention", "sliding_attention",
                        "sliding_attention", "sliding_attention"],
    })
}

// T14 (found live 2026-10-02): vLLM 0.30 keeps the draft model's KV layers
// in the same pool as the checkpoint's, so a context fitted to the
// checkpoint's layers alone does not fit the grant with DFlash2. The draft
// model's KV per token is counted with the checkpoint's.
#[test]
fn a_draft_models_kv_layers_are_counted_with_the_checkpoints() {
    let drafter = dflash_drafter();
    let mut with_draft = inputs(4 * GIB);
    with_draft.draft = Some(Ok(&drafter));
    // 512 KiB (checkpoint) + 2 × 5 × 8 × 128 × 2 = 20 KiB (draft) per token.
    let per_token = 512 * 1024 + 20 * 1024;
    let raw = (4 * GIB) as u64 / per_token;
    let fit = fit_context(with_draft, Ok(&dense()));
    assert_eq!(fit.tokens, Some((raw - raw % 16) as u32));
    assert_eq!(fit.source, ContextSource::Fitted);
    assert!(fit.reason.unwrap().contains("draft model"));
    // One KV dtype applies to both: fp8 halves both.
    with_draft.kv_cache_dtype = Some("fp8");
    let raw = (4 * GIB) as u64 / (per_token / 2);
    assert_eq!(
        fit_context(with_draft, Ok(&dense())).tokens,
        Some((raw - raw % 16) as u32)
    );
    // A draft model whose configuration cannot be read falls back.
    let mut unreadable = inputs(4 * GIB);
    unreadable.draft = Some(Err("the draft model has no readable config.json"));
    let fit = fit_context(unreadable, Ok(&dense()));
    assert_eq!(fit.source, ContextSource::Fallback);
    assert!(fit.reason.unwrap().contains("draft model"));
    // A declared context the pair cannot hold warns only for an exact shape;
    // the sliding draft model is approximate, so no warning.
    let mut declared = inputs(4 * GIB);
    declared.declared = Some(8192);
    declared.draft = Some(Ok(&drafter));
    assert_eq!(fit_context(declared, Ok(&dense())).warning, None);
}

/// Qwen3.8-27B NVFP4's language model as its `config.json` states it: 48
/// gated-delta-net (linear attention) layers and 16 attention layers with 4
/// KV heads of 256.
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

fn vllm_hybrid(kv: i64, speculative_tokens: Option<u32>, sequences: u32) -> FitInputs<'static> {
    let mut fit = inputs(kv);
    fit.kv_cache_dtype = Some("fp8");
    fit.reserved_blocks = VLLM_RESERVED_BLOCKS;
    fit.vllm = Some(VllmFit {
        sequences,
        speculative_tokens,
    });
    fit
}

// T14 (owner decision 2026-10-02, found live): on a hybrid checkpoint vLLM
// sizes its pool in blocks shared by every layer group, padded so an
// attention page holds one recurrent state, and needs one state block per
// sequence plus 2 + speculative-blocks per recurrent group for a request. At
// the default 4 GiB with fp8 KV, vLLM 0.30 held 83 blocks without DFlash2
// (block 1568 tokens) and 254 with it (block 1648 tokens), and the 32752
// tokens fitted per token were refused with DFlash2 (26368 at most). The fit
// follows vLLM's block layout for such a checkpoint, for 32 sequences.
#[test]
fn a_hybrid_model_on_vllm_is_fitted_to_its_block_layout() {
    let target = qwen38_27b();
    let drafter = dflash_drafter();
    // Without a draft model: 16-layer groups (1 attention, 3 recurrent),
    // 82 usable blocks of 1568 tokens, two state blocks per recurrent group.
    let fit = fit_context(vllm_hybrid(4 * GIB, None, 32), Ok(&target));
    assert_eq!(fit.source, ContextSource::Fitted);
    assert_eq!(fit.tokens, Some(75 * 1568));
    assert!(fit.reason.unwrap().contains("vLLM"));
    // With DFlash2 at 7 speculative tokens: 5-layer groups (4 attention, 10
    // recurrent, 1 draft), 1648-token blocks, 2 + 16 state blocks per
    // recurrent group.
    let mut with_draft = vllm_hybrid(4 * GIB, Some(7), 32);
    with_draft.draft = Some(Ok(&drafter));
    let fit = fit_context(with_draft, Ok(&target));
    assert_eq!(fit.source, ContextSource::Fitted);
    let tokens = fit.tokens.unwrap();
    assert_eq!(tokens, 14 * 1648);
    assert!(tokens <= 26368, "vLLM held at most 26368");
    // More sequences than the pool holds state blocks for: refused with the
    // KV cache that would hold them.
    let fit = fit_context(vllm_hybrid(4 * GIB, None, 256), Ok(&target));
    assert_eq!(fit.source, ContextSource::Fallback);
    let reason = fit.reason.unwrap();
    assert!(
        reason.contains("256 sequences") && reason.contains("memory.kv_cache"),
        "{reason}"
    );
    // SGLang (no vLLM layout) and a non-hybrid model keep the per-token fit.
    let mut sglang = inputs(4 * GIB);
    sglang.kv_cache_dtype = Some("fp8");
    assert_eq!(fit_context(sglang, Ok(&target)).tokens, Some(32768));
    assert_eq!(
        fit_context(vllm_hybrid(4 * GIB, None, 32), Ok(&dense())).tokens,
        fit_context(
            FitInputs {
                vllm: None,
                ..vllm_hybrid(4 * GIB, None, 32)
            },
            Ok(&dense())
        )
        .tokens
    );
}

// T14 (amendment A14): SGLang keeps a gated-delta-net hybrid's recurrent
// state in its own pool, so its context is fitted to the attention layers'
// KV alone: 16 × 2 × 4 × 256 fp8 bytes per token, 131072 tokens in 4 GiB.
#[test]
fn a_hybrid_model_on_sglang_is_fitted_to_its_attention_layers() {
    let target = qwen38_27b();
    let mut sglang = inputs(4 * GIB);
    sglang.kv_cache_dtype = Some("fp8");
    sglang.sglang = true;
    let fit = fit_context(sglang, Ok(&target));
    assert_eq!(fit.tokens, Some(131072));
    assert_eq!(fit.source, ContextSource::Fitted);
    assert_eq!(fit.reason, None);
    // A dense model is fitted as before.
    let mut dense_fit = inputs(4 * GIB);
    dense_fit.sglang = true;
    assert_eq!(fit_context(dense_fit, Ok(&dense())).tokens, Some(8192));
}
