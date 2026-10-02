//! Owner decision 2026-10-02: the fit of a hybrid checkpoint on vLLM.
//!
//! vLLM 0.29 and 0.30 keep a hybrid model's attention KV and its recurrent
//! (gated delta net) state in one pool of uniform pages
//! (`vllm/v1/core/kv_cache_utils.py`, `_get_kv_cache_groups_uniform_page_size`):
//!
//! - the attention block is lengthened, in 16-token steps, until one
//!   attention page holds one recurrent state;
//! - layers are grouped by kind into groups of equal size (the smallest kind's
//!   count, or the largest when it is under 1.5 times the smallest, padded),
//!   and a block holds one page of every layer of a group;
//! - every sequence needs a state block (`max_num_seqs` at most the blocks),
//!   and a request needs, per attention group, its context in blocks, and per
//!   recurrent group 2 + speculative state blocks (`MambaSpec`, `align`
//!   cache mode).
//!
//! Found live 2026-10-02 with Qwen3.8-27B NVFP4 and fp8 KV at 4 GiB: 83 blocks
//! of 1568 tokens without a draft model and 254 of 1648 tokens with DFlash2
//! (7 speculative tokens, 16 speculative state blocks), which held 26368
//! tokens at most. The per-token fit had given 32752. This module reproduces
//! that layout from the configuration, conservatively: a sliding-window draft
//! layer is counted as holding the whole context, the speculative state blocks
//! are taken as 2 × (tokens + 1), and two blocks are left unused. A shape it
//! does not recognise returns `None`, and the per-token fit applies.

use serde_json::Value;

use super::{dtype_bytes, positive, KvShape};

/// What the vLLM layout needs besides the configurations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VllmFit {
    /// The `--max-num-seqs` the launch runs with.
    pub sequences: u32,
    /// `--speculative-config`'s `num_speculative_tokens`, when speculative
    /// decoding is on.
    pub speculative_tokens: Option<u32>,
}

/// Blocks never used for the fit: vLLM's null block and one of slack.
const UNUSED_BLOCKS: u64 = 2;

fn cdiv(a: u64, b: u64) -> u64 {
    a.div_ceil(b)
}

/// The recurrent state of one gated-delta-net layer, in bytes (the
/// convolution state, in the model's dtype, keeps `kernel - 1 + speculative`
/// positions; the temporal state is in `mamba_ssm_dtype`).
fn gated_delta_state(text: &Value, speculative: u64) -> Option<u64> {
    let key_heads = positive(text, "linear_num_key_heads")?;
    let key_dim = positive(text, "linear_key_head_dim")?;
    let value_heads = positive(text, "linear_num_value_heads")?;
    let value_dim = positive(text, "linear_value_head_dim")?;
    let kernel = positive(text, "linear_conv_kernel_dim")?;
    let model_dtype = ["dtype", "torch_dtype"]
        .iter()
        .find_map(|key| text.get(*key).and_then(Value::as_str))
        .and_then(dtype_bytes)?;
    let ssm_dtype = match text.get("mamba_ssm_dtype").and_then(Value::as_str) {
        Some(name) => dtype_bytes(name)?,
        None => model_dtype,
    };
    let conv_dim = key_heads * key_dim * 2 + value_heads * value_dim;
    let conv = conv_dim * (kernel - 1 + speculative) * model_dtype;
    let ssm = value_heads * key_dim * value_dim * ssm_dtype;
    Some(conv + ssm)
}

/// The largest context vLLM's layout holds, or why the grant cannot hold
/// the sequences; `None` when the checkpoint is not a recognised hybrid.
pub(super) fn fitted_tokens(
    config: &Value,
    shape: &KvShape,
    draft: Option<&KvShape>,
    element: u64,
    grant: u64,
    fit: VllmFit,
) -> Option<Result<u64, String>> {
    let text = match config.get("text_config") {
        Some(text) if text.is_object() && config.get("num_hidden_layers").is_none() => text,
        _ => config,
    };
    let types = text.get("layer_types")?.as_array()?;
    let count = |kind: &str| types.iter().filter(|t| t.as_str() == Some(kind)).count() as u64;
    let (attention, recurrent) = (count("full_attention"), count("linear_attention"));
    if attention == 0 || recurrent == 0 || attention + recurrent != types.len() as u64 {
        return None;
    }
    let speculative = u64::from(fit.speculative_tokens.unwrap_or(0));
    let state = gated_delta_state(text, speculative)?;
    let per_token = 2 * shape.kv_heads * shape.head_dim * element;
    let block = cdiv(cdiv(state, per_token), 16) * 16;
    let page = block * per_token;
    // A draft model's layers form a kind of their own; vLLM needs one page
    // size, so another shape is not modelled here.
    let draft_layers = match draft {
        Some(draft) if 2 * draft.kv_heads * draft.head_dim * element != per_token => return None,
        Some(draft) => draft.layers,
        None => 0,
    };
    let kinds: Vec<u64> = [attention, recurrent, draft_layers]
        .into_iter()
        .filter(|n| *n > 0)
        .collect();
    let (min, max) = (*kinds.iter().min()?, *kinds.iter().max()?);
    let group = if max * 2 < min * 3 { max } else { min };
    let bytes_per_block = group * page;
    let blocks = grant / bytes_per_block;
    let needed = |sequences: u64| (sequences + UNUSED_BLOCKS) * bytes_per_block;
    if blocks < u64::from(fit.sequences) + UNUSED_BLOCKS {
        return Some(Err(format!(
            "vLLM keeps one recurrent-state block of {bytes_per_block} bytes per sequence for \
             this hybrid model; {} sequences need a KV cache of at least {} bytes, above the \
             {grant}-byte grant: raise memory.kv_cache or lower max_concurrent_requests",
            fit.sequences,
            needed(u64::from(fit.sequences))
        )));
    }
    let usable = blocks - UNUSED_BLOCKS;
    let speculative_blocks = if speculative > 0 {
        2 * (speculative + 1)
    } else {
        0
    };
    let fixed = cdiv(recurrent, group) * (2 + speculative_blocks)
        + if draft_layers > 0 {
            cdiv(draft_layers, group)
        } else {
            0
        };
    let per_block = cdiv(attention, group) + cdiv(draft_layers, group);
    let context_blocks = usable.saturating_sub(fixed) / per_block;
    let tokens = (context_blocks * block).min(shape.max_position);
    let tokens = tokens - tokens % 16;
    if tokens == 0 {
        return Some(Err(format!(
            "vLLM's recurrent state for one request of this hybrid model leaves no room for \
             context in the {grant}-byte grant; raise memory.kv_cache"
        )));
    }
    Some(Ok(tokens))
}
