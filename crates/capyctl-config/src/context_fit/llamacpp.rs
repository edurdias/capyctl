//! ADR 0029 §9: a llama.cpp deployment's memory request from its GGUF header.
//!
//! llama-server allocates every slot's whole KV cache at start: CapyCTL
//! renders `--ctx-size <pad256(context_length) × slots> --parallel <slots>
//! --no-kv-unified` (ADR 0029 §5), so the cache holds one stream of
//! `pad256(context_length)` cells per slot in every layer that keeps one
//! (llama.cpp 0.6.0 `src/llama-context.cpp` `n_ctx_seq`,
//! `src/llama-kv-cache.cpp`). Its size is
//!
//! ```text
//! slots × pad256(context_length) × Σ_layers head_count_kv × (key_length × bytes(K) + value_length × bytes(V))
//! ```
//!
//! read from `<arch>.block_count`, `<arch>.attention.head_count_kv` (a number
//! or one per layer), `<arch>.attention.key_length` and `.value_length`
//! (else `embedding_length / head_count`), with the cache types' block sizes.
//! The request is then the launch's weights (the rendered GGUF and its
//! shards, the projector and the draft model, ADR 0014 amendment A6) plus
//! that cache plus the family margin (ADR 0014 §5, amendment A18, ADR 0019
//! §3), through the derivation every engine shares.
//!
//! Where the header or the arguments say the cache is laid out otherwise, or
//! that weights or cache leave the GPU, nothing is derived and the deployment
//! states `memory.kv_cache` or `resources`.

use std::path::{Path, PathBuf};

use capyctl_domain::gguf::{GgufFacts, GgufKv, GgufKvRefusal, GgufKvShape};
use capyctl_domain::launch::LlamacppGpuLayers;

use crate::engine_policy::parse_options;
use crate::gguf::GgufHeader;

/// llama.cpp 0.6.0 `llm_arch_is_recurrent` (`src/llama-arch.cpp`).
const RECURRENT_ARCHITECTURES: &[&str] =
    &["mamba", "mamba2", "rwkv6", "rwkv6qwen2", "rwkv7", "arwkv7"];

/// llama.cpp 0.6.0 `llm_arch_is_hybrid`.
const HYBRID_ARCHITECTURES: &[&str] = &[
    "jamba",
    "falcon-h1",
    "plamo2",
    "granitehybrid",
    "lfm2",
    "lfm2moe",
    "nemotron_h",
    "nemotron_h_moe",
    "qwen3next",
    "kimi-linear",
    "bailingmoe3",
    "kimi-k3",
    "glm5-next",
    "qwen35",
    "qwen35moe",
    "qwen4exp",
    "deepseek4",
    "minimax-01",
];

/// llama.cpp 0.6.0 architectures whose model code sets sliding-window (or
/// chunked) layers whatever the header says (`src/models/*.cpp`): no
/// `sliding_window` key is needed for them to have such layers.
const SLIDING_WINDOW_ARCHITECTURES: &[&str] = &[
    "gemma2",
    "gemma3",
    "gemma3n",
    "gemma4",
    "gemma4-assistant",
    "gemma-embedding",
    "cohere2",
    "cohere2moe",
    "gpt-oss",
    "granite_swa",
    "dots3note",
    "exaone-moe",
    "maple",
    "mimo2",
    "muse-glimmer",
    "spark2_5",
    "step35",
];

/// llama.cpp 0.6.0 architectures `llama_model::create_memory` gives no cache
/// or a cache of its own kind (embedding and diffusion models, an indexer
/// cache, encoder-decoder models), and the projector's.
const LAYOUT_ARCHITECTURES: &[&str] = &[
    "bert",
    "jina-bert-v2",
    "jina-bert-v3",
    "nomic-bert",
    "nomic-bert-moe",
    "neo-bert",
    "eurobert",
    "wavtokenizer-dec",
    "modern-bert",
    "dream",
    "llada",
    "llada-moe",
    "rnd1",
    "clef",
    "minimax-m3",
    "glm-dsa",
    "deepseek32",
    "hy_v4",
    "dflash",
    "eagle3",
    "t5",
    "t5encoder",
    "clip",
];

/// The cache shape the header describes, and its training context.
pub fn kv_of_header(header: &GgufHeader) -> (Option<u32>, GgufKv) {
    let Some(arch) = header.architecture() else {
        return (None, GgufKv::Refused(GgufKvRefusal::Unreadable));
    };
    let key = |name: &str| format!("{arch}.{name}");
    let training_context = header
        .uint(&key("context_length"))
        .and_then(|tokens| u32::try_from(tokens).ok())
        .filter(|tokens| *tokens > 0);
    (training_context, kv_shape(header, arch))
}

fn kv_shape(header: &GgufHeader, arch: &str) -> GgufKv {
    use GgufKvRefusal::*;
    let key = |name: &str| format!("{arch}.{name}");
    let refused = |refusal| GgufKv::Refused(refusal);
    if HYBRID_ARCHITECTURES.contains(&arch)
        || header.keys.contains(&key("full_attention_interval"))
        || header.names_under(&key("shortconv"))
    {
        return refused(Hybrid);
    }
    if RECURRENT_ARCHITECTURES.contains(&arch)
        || header.names_under(&key("ssm"))
        || header.names_under(&key("wkv"))
    {
        return refused(Recurrent);
    }
    if header.keys.contains(&key("attention.kv_lora_rank")) {
        return refused(Mla);
    }
    // llama4 has chunked attention unless the header sets a window of 0
    // (`src/models/llama4.cpp`); elsewhere a positive window or a pattern
    // means sliding-window layers.
    let window = header.uint(&key("attention.sliding_window"));
    if SLIDING_WINDOW_ARCHITECTURES.contains(&arch)
        || (arch == "llama4" && window != Some(0))
        || window.is_some_and(|tokens| tokens > 0)
        || header
            .keys
            .contains(&key("attention.sliding_window_pattern"))
    {
        return refused(SlidingWindow);
    }
    if LAYOUT_ARCHITECTURES.contains(&arch) || header.names_under(&key("attention.indexer")) {
        return refused(Layout);
    }
    attention_shape(header, arch).map_or(refused(Unreadable), GgufKv::Attention)
}

/// `llama_model::load_hparams` for the attention shape: per-layer heads and
/// KV heads (the KV heads default to the heads), the head widths (default
/// `embedding_length / head_count` of the first layer), and the layers the
/// main context keeps a cache for (`block_count` less the MTP layers,
/// `nextn_predict_layers`, which only an MTP context caches).
fn attention_shape(header: &GgufHeader, arch: &str) -> Option<GgufKvShape> {
    let key = |name: &str| format!("{arch}.{name}");
    let blocks = usize::try_from(header.uint(&key("block_count"))?)
        .ok()
        .filter(|blocks| (1..=4096).contains(blocks))?;
    let heads = header.per_layer(&key("attention.head_count"), blocks)?;
    let kv_heads = match header.value(&key("attention.head_count_kv")) {
        Some(_) => header.per_layer(&key("attention.head_count_kv"), blocks)?,
        None => heads.clone(),
    };
    let first_heads = *heads.first().filter(|heads| **heads > 0)?;
    let width = |name: &str| match header.value(&key(name)) {
        Some(_) => header.uint(&key(name)),
        None => header
            .uint(&key("embedding_length"))
            .map(|embedding| embedding / first_heads),
    };
    let key_length = width("attention.key_length").filter(|width| *width > 0)?;
    let value_length = width("attention.value_length").filter(|width| *width > 0)?;
    let nextn = header.uint(&key("nextn_predict_layers")).unwrap_or(0);
    let nextn = usize::try_from(nextn)
        .ok()
        .filter(|nextn| *nextn <= blocks)?;
    let layers = if nextn > 0 && nextn < blocks {
        blocks - nextn
    } else {
        blocks
    };
    let mut k_values = 0u64;
    let mut v_values = 0u64;
    let mut v_widest = 0u64;
    for heads in &kv_heads[..layers] {
        let v_row = heads.checked_mul(value_length)?;
        k_values = k_values.checked_add(heads.checked_mul(key_length)?)?;
        v_values = v_values.checked_add(v_row)?;
        v_widest = v_widest.max(v_row);
    }
    let layers = u32::try_from(layers).ok()?;
    let shape = GgufKvShape {
        layers,
        k_values,
        v_values,
        v_values_padded: v_widest.checked_mul(u64::from(layers))?,
    };
    (k_values > 0 && v_values > 0).then_some(shape)
}

/// ADR 0029 §9: what a llama.cpp launch loads beside its checkpoint, as the
/// deployment names it, for [`measure`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlamacppFiles {
    /// `engine_config.llamacpp.gguf_file`.
    pub gguf_file: Option<String>,
    /// `engine_config.llamacpp.mmproj_file`.
    pub mmproj_file: Option<String>,
    /// ADR 0029 §8: the draft model the arguments name (`--model-draft`).
    pub draft: Option<String>,
    /// `security.approved_paths`, which the draft model must lie in.
    pub approved_paths: Vec<String>,
}

impl LlamacppFiles {
    /// The files of a llama.cpp launch whose installation and deployment
    /// arguments are `args`.
    pub fn new(
        gguf_file: Option<String>,
        mmproj_file: Option<String>,
        args: &[String],
        approved_paths: &[String],
    ) -> Self {
        Self {
            gguf_file,
            mmproj_file,
            draft: draft_model(args),
            approved_paths: approved_paths.to_vec(),
        }
    }
}

/// Every spelling of llama-server's draft model option.
const DRAFT_MODEL: &[&str] = &["--model-draft", "--spec-draft-model"];

/// ADR 0029 §8: the draft model the arguments name, the last one llama.cpp
/// would take.
pub fn draft_model(args: &[String]) -> Option<String> {
    parse_options(args)
        .ok()?
        .into_iter()
        .filter(|option| DRAFT_MODEL.contains(&option.name.as_str()))
        .filter_map(|option| option.value)
        .next_back()
}

/// ADR 0029 §9: measure a llama.cpp launch's GGUF facts on the machine that
/// holds `checkpoint`: the bytes it loads and its header's cache shape. Reads
/// file sizes and the one header, never tensor data. A checkpoint with no
/// model to pick, or a projector that is not there, is
/// [`GgufKvRefusal::NoModel`]; the launch refuses it too.
pub fn measure(checkpoint: &Path, files: &LlamacppFiles) -> GgufFacts {
    let no_model = GgufFacts {
        weights_bytes: 0,
        training_context: None,
        kv: GgufKv::Refused(GgufKvRefusal::NoModel),
    };
    let Ok(pick) = crate::checkpoint_layout::pick_gguf(checkpoint, files.gguf_file.as_deref())
    else {
        return no_model;
    };
    let mut loaded: Vec<PathBuf> = pick
        .files
        .iter()
        .map(|file| checkpoint.join(file))
        .collect();
    if let Some(mmproj) = &files.mmproj_file {
        match crate::checkpoint_layout::projector_file(checkpoint, mmproj) {
            Ok(relative) => loaded.push(checkpoint.join(relative)),
            Err(_) => return no_model,
        }
    }
    // ADR 0014 amendment A6, ADR 0029 §8: a draft model is counted where it
    // still lies inside an approved path once symlinks resolve (the launch
    // refuses one that does not).
    if let Some(draft) = files.draft.as_deref().and_then(|draft| {
        let real = std::fs::canonicalize(draft).ok()?;
        files
            .approved_paths
            .iter()
            .filter_map(|root| std::fs::canonicalize(root).ok())
            .any(|root| real.starts_with(root))
            .then_some(real)
    }) {
        loaded.push(draft);
    }
    let mut weights_bytes = 0i64;
    for path in &loaded {
        let Some(sum) = std::fs::metadata(path)
            .ok()
            .filter(std::fs::Metadata::is_file)
            .and_then(|metadata| i64::try_from(metadata.len()).ok())
            .and_then(|bytes| weights_bytes.checked_add(bytes))
        else {
            return no_model;
        };
        weights_bytes = sum;
    }
    let (training_context, kv) = match crate::gguf::read_header_file(&checkpoint.join(&pick.file)) {
        Ok(header) => kv_of_header(&header),
        Err(_) => (None, GgufKv::Refused(GgufKvRefusal::Unreadable)),
    };
    GgufFacts {
        weights_bytes,
        training_context,
        kv,
    }
}

/// ADR 0029 §5: the bytes one block of `values` cache values takes in a
/// llama.cpp cache type (`ggml-common.h`: `q8_0` is 34 bytes per 32).
pub fn cache_type_block(cache_type: &str) -> Option<(u64, u64)> {
    Some(match cache_type {
        "f32" => (1, 4),
        "f16" | "bf16" => (1, 2),
        "q8_0" => (32, 34),
        "q4_0" | "iq4_nl" => (32, 18),
        "q4_1" => (32, 20),
        "q5_0" => (32, 22),
        "q5_1" => (32, 24),
        _ => return None,
    })
}

/// ADR 0029 §9: the KV cache llama-server allocates for `shape` with
/// `slots` slots of `context_length` tokens in `cache_type` (keys and
/// values both). Without flash attention known to be on, the V cache is
/// counted padded to its widest layer, as llama.cpp lays it out then.
pub fn kv_cache_bytes(
    shape: &GgufKvShape,
    context_length: u32,
    slots: u32,
    cache_type: &str,
    flash_attention: bool,
) -> Option<i64> {
    let (block, bytes) = cache_type_block(cache_type)?;
    let row = |values: u64| values.div_ceil(block).checked_mul(bytes);
    let v_values = if flash_attention {
        shape.v_values
    } else {
        shape.v_values_padded
    };
    let per_token = row(shape.k_values)?.checked_add(row(v_values)?)?;
    let cells = u64::from(crate::llamacpp::slot_pool_tokens(context_length, slots)?);
    i64::try_from(per_token.checked_mul(cells)?).ok()
}

/// Whether the arguments turn flash attention on (`--flash-attn on`; its
/// default `auto` may leave it off).
pub fn flash_attention_on(args: &[String]) -> bool {
    parse_options(args).is_ok_and(|options| {
        options
            .into_iter()
            .filter(|option| option.name == "--flash-attn")
            .filter_map(|option| option.value)
            .next_back()
            .is_some_and(|value| matches!(value.as_str(), "on" | "enabled" | "true" | "1"))
    })
}

/// Options that keep weights or the KV cache off the GPU, every long
/// spelling (llama-server 0.6.0 `common/arg.cpp`).
const OFFLOADING: &[&str] = &[
    "--override-tensor",
    "--cpu-moe",
    "--n-cpu-moe",
    "--n-cpu-ffn",
    "--no-kv-offload",
];

/// ADR 0029 §9: why the arguments (installation's, then deployment's) or the
/// layer count keep the KV cache from being derived, if they do: weights or
/// cache off the GPU, an `mlock` load mode, or a draft context (a draft
/// model or a `draft-*` speculative type) whose own cache is not derived.
pub fn derivation_refusal(n_gpu_layers: LlamacppGpuLayers, args: &[String]) -> Option<String> {
    if let LlamacppGpuLayers::Count(layers) = n_gpu_layers {
        return Some(format!(
            "engine_config.llamacpp.n_gpu_layers {layers} keeps layers off the GPU"
        ));
    }
    let options = parse_options(args).ok()?;
    for option in &options {
        let name = option.name.as_str();
        let value = option.value.as_deref().unwrap_or_default();
        if OFFLOADING.contains(&name) {
            return Some(format!(
                "`{name}` keeps weights or the KV cache off the GPU"
            ));
        }
        if name == "--load-mode" && value.split('+').any(|mode| mode == "mlock") {
            return Some(format!(
                "`--load-mode {value}` locks a host copy of the weights"
            ));
        }
        if DRAFT_MODEL.contains(&name) {
            return Some(format!(
                "`{name}` loads a draft model whose own context keeps a KV cache"
            ));
        }
        if name == "--spec-type" {
            if let Some(kind) = value.split(',').find(|kind| kind.starts_with("draft-")) {
                return Some(format!(
                    "`--spec-type {kind}` keeps a draft context whose KV cache is not measured"
                ));
            }
        }
    }
    None
}
