//! ADR 0014 §5 (owner decision 2026-09-25): fit a deployment's context to its
//! KV cache grant.
//!
//! A deployment that states no `engine_config.context_length` used to leave
//! the engine to take the model's own maximum, which on a small grant makes
//! the engine refuse to start (the pool cannot hold one maximal sequence).
//! capyctl now computes the largest context the grant holds from the model's
//! `config.json`, caps it at the model's `max_position_embeddings`, rounds it
//! down to a block multiple and passes it as the engine's maximum model length
//! (vLLM `--max-model-len`, SGLang `--context-length`).
//!
//! The computation is conservative. Sliding-window and hybrid layers are
//! counted as full attention (more KV per token, so a shorter context), and a
//! shape that cannot be read reliably (multi-head latent attention, missing
//! fields, an unknown KV dtype) falls back to [`FALLBACK_CONTEXT`] with the
//! reason recorded for status.
//!
//! A speculative deployment's draft model keeps its own KV layers in the
//! engine's pool (vLLM groups them with the checkpoint's; SGLang sizes a draft
//! pool beside the target's), so its KV per token, read from the draft
//! model's own `config.json`, is added to the checkpoint's (found live
//! 2026-10-02: vLLM 0.30 with DFlash2 refused the context fitted to the
//! checkpoint's layers alone).
//!
//! The fit is made where the checkpoint is read, at launch render: on the
//! embedded host for standalone and on the host agent for a remote host. It is
//! not part of the effective configuration, so re-resolving a stored revision
//! never depends on a file.

use std::path::Path;

use capyctl_domain::launch::LaunchSettings;
use serde::Serialize;
use serde_json::Value;

use crate::engine_policy::{option_names, typed_field_option, Engine};

mod sglang_pool;
mod vllm_hybrid;
pub use sglang_pool::{
    sglang_pool, sglang_pool_for_effective, sglang_pool_for_launch, static_pool_bytes, SglangPool,
    STATE_SLOTS_PER_REQUEST,
};
pub use vllm_hybrid::VllmFit;

/// The context used when the KV bytes per token cannot be computed reliably.
pub const FALLBACK_CONTEXT: u32 = 4096;

/// The block the fitted context is rounded down to when the deployment names
/// none (vLLM's default block size; a multiple of SGLang's page size).
pub const DEFAULT_BLOCK_TOKENS: u32 = 16;

/// The KV blocks a vLLM fit leaves to the engine. vLLM keeps a null block out
/// of its pool, so a context that uses every whole block of the grant is
/// refused at start. Found live on a 16 GB discrete GPU: vLLM 0.29 held one
/// block fewer than the grant divided by the block size.
pub const VLLM_RESERVED_BLOCKS: u32 = 1;

/// The largest `config.json` read. A model configuration is a few kilobytes.
const MAX_CONFIG_BYTES: u64 = 4 << 20;

/// Where the effective context came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextSource {
    /// The deployment's `engine_config.context_length`.
    Declared,
    /// The installation's host-fixed arguments set the maximum model length;
    /// capyctl passes nothing and the host's value applies.
    HostFixed,
    /// Computed from the model configuration and the KV cache grant.
    Fitted,
    /// The KV bytes per token could not be computed reliably.
    Fallback,
    /// Undeclared, and the checkpoint is on a remote host: that host fits the
    /// context from its own copy when it renders the launch.
    OnHost,
}

/// The effective context of one launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContextFit {
    /// The value capyctl passes to the engine; `None` for a host-fixed one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u32>,
    pub source: ContextSource,
    /// Why the fit fell back, or how the shape was approximated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// A declared context the grant provably cannot hold. The launch is not
    /// refused here (the engine refuses it, as before); this says why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    /// Amendment A14: the running requests SGLang is held to when a hybrid
    /// model's state cache holds fewer than the deployment's count (or
    /// CapyCTL's in-flight bound).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub running_limit: Option<u32>,
    /// ADR 0023 §4 (amended 2026-10-03): the requests a TensorFold launch
    /// decodes together (`--parallel`) and where the count came from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub streams: Option<Streams>,
}

/// ADR 0023 §4 (amended 2026-10-03): TensorFold's `--parallel` for one launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Streams {
    /// The requests decoded together; `None` when a passed value does not
    /// parse (TensorFold refuses it at start).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    pub source: StreamsSource,
    /// Why the count is not what the launch passes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Where a TensorFold launch's stream count came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamsSource {
    /// The deployment's `engine_config.max_concurrent_requests`.
    Declared,
    /// CapyCTL's TensorFold default (`TENSORFOLD_DEFAULT_PARALLEL`).
    Default,
    /// `--parallel` among the deployment's extra arguments.
    ExtraArgs,
    /// `--parallel` among the installation's host-fixed arguments.
    HostFixed,
}

/// The per-token KV footprint read from a model configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KvShape {
    pub layers: u64,
    pub kv_heads: u64,
    pub head_dim: u64,
    /// The checkpoint's own dtype width, when it names one.
    pub model_dtype_bytes: Option<u64>,
    pub max_position: u64,
    /// `None` when every layer is plain full attention, so the count is exact;
    /// otherwise how it was made conservative.
    pub approximation: Option<&'static str>,
}

fn positive(config: &Value, key: &str) -> Option<u64> {
    config.get(key)?.as_u64().filter(|value| *value > 0)
}

fn dtype_bytes(name: &str) -> Option<u64> {
    match name.to_ascii_lowercase().as_str() {
        "bfloat16" | "bf16" | "float16" | "fp16" | "half" => Some(2),
        "float32" | "fp32" | "float" => Some(4),
        name if name.starts_with("fp8") => Some(1),
        _ => None,
    }
}

impl KvShape {
    /// Read the shape from a parsed `config.json`. A multimodal checkpoint
    /// keeps the language model's shape under `text_config`.
    pub fn from_config(config: &Value) -> Result<Self, String> {
        let config = match config.get("text_config") {
            Some(text) if text.is_object() && config.get("num_hidden_layers").is_none() => text,
            _ => config,
        };
        if config.get("kv_lora_rank").is_some_and(|v| !v.is_null()) {
            return Err(
                "multi-head latent attention: the engine's KV layout is not \
                        computable from the configuration"
                    .into(),
            );
        }
        let layers = positive(config, "num_hidden_layers")
            .ok_or("the model configuration names no num_hidden_layers")?;
        let heads = positive(config, "num_attention_heads")
            .ok_or("the model configuration names no num_attention_heads")?;
        let kv_heads = match config.get("num_key_value_heads") {
            None | Some(Value::Null) => heads,
            Some(_) => positive(config, "num_key_value_heads")
                .ok_or("num_key_value_heads is not a positive integer")?,
        };
        let head_dim = match positive(config, "head_dim") {
            Some(dim) => dim,
            None => {
                let hidden = positive(config, "hidden_size")
                    .ok_or("the model configuration names no head_dim or hidden_size")?;
                if hidden % heads != 0 {
                    return Err("hidden_size is not a multiple of num_attention_heads".into());
                }
                hidden / heads
            }
        };
        let max_position = positive(config, "max_position_embeddings")
            .ok_or("the model configuration names no max_position_embeddings")?;
        let model_dtype_bytes = ["torch_dtype", "dtype"]
            .iter()
            .find_map(|key| config.get(*key).and_then(Value::as_str))
            .and_then(dtype_bytes);
        // Conservative: sliding-window and non-attention layers are counted as
        // full attention, which overstates the KV per token.
        let layer_types = config.get("layer_types").and_then(Value::as_array);
        let hybrid = layer_types.is_some_and(|types| {
            types
                .iter()
                .any(|kind| !matches!(kind.as_str(), Some("full_attention" | "sliding_attention")))
        }) || [
            "layers_block_type",
            "hybrid_override_pattern",
            "attn_layer_period",
        ]
        .iter()
        .any(|key| config.get(*key).is_some_and(|v| !v.is_null()));
        let sliding = layer_types.is_some_and(|types| {
            types
                .iter()
                .any(|kind| kind.as_str() == Some("sliding_attention"))
        }) || (config.get("sliding_window").is_some_and(|v| !v.is_null())
            && config.get("use_sliding_window").and_then(Value::as_bool) != Some(false));
        let approximation = if hybrid {
            Some("hybrid model: every layer counted as full attention")
        } else if sliding {
            Some("sliding-window attention: every layer counted as full attention")
        } else {
            None
        };
        Ok(Self {
            layers,
            kv_heads,
            head_dim,
            model_dtype_bytes,
            max_position,
            approximation,
        })
    }

    /// Key plus value bytes for one token across every layer.
    pub fn bytes_per_token(&self, element_bytes: u64) -> Option<u64> {
        2u64.checked_mul(self.layers)?
            .checked_mul(self.kv_heads)?
            .checked_mul(self.head_dim)?
            .checked_mul(element_bytes)
    }
}

/// Everything the fit reads besides the model configuration.
#[derive(Debug, Clone, Copy)]
pub struct FitInputs<'a> {
    pub declared: Option<u32>,
    pub kv_cache_bytes: i64,
    pub kv_cache_dtype: Option<&'a str>,
    pub dtype: Option<&'a str>,
    pub block_tokens: Option<u32>,
    /// Whole blocks of the grant the engine keeps for itself
    /// ([`VLLM_RESERVED_BLOCKS`] for vLLM).
    pub reserved_blocks: u32,
    /// The draft model's parsed `config.json` (or why it could not be read),
    /// when the launch loads one beside the checkpoint.
    pub draft: Option<Result<&'a Value, &'a str>>,
    /// The launch is vLLM's: a hybrid checkpoint is fitted to its block
    /// layout (owner decision 2026-10-02).
    pub vllm: Option<VllmFit>,
    /// The launch is SGLang's: a gated-delta-net hybrid keeps KV for its
    /// full-attention layers only (amendment A14).
    pub sglang: bool,
}

/// The width of one cached element: an explicit KV dtype (fp8 variants are one
/// byte), else the deployment's dtype, else the checkpoint's.
fn element_bytes(inputs: &FitInputs<'_>, shape: &KvShape) -> Result<u64, String> {
    match inputs.kv_cache_dtype {
        Some(kv) if !kv.eq_ignore_ascii_case("auto") => {
            return dtype_bytes(kv).ok_or_else(|| format!("unknown KV cache dtype `{kv}`"));
        }
        _ => {}
    }
    match inputs.dtype {
        Some(dtype) if !dtype.eq_ignore_ascii_case("auto") => {
            dtype_bytes(dtype).ok_or_else(|| format!("unknown dtype `{dtype}`"))
        }
        _ => shape
            .model_dtype_bytes
            .ok_or_else(|| "the model configuration names no dtype".into()),
    }
}

fn fallback(reason: impl Into<String>) -> ContextFit {
    ContextFit {
        tokens: Some(FALLBACK_CONTEXT),
        source: ContextSource::Fallback,
        reason: Some(reason.into()),
        warning: None,
        running_limit: None,
        streams: None,
    }
}

/// The draft model's KV shape, when the launch loads one.
fn draft_shape(inputs: &FitInputs<'_>) -> Result<Option<KvShape>, String> {
    match inputs.draft {
        None => Ok(None),
        Some(config) => config
            .map_err(str::to_owned)
            .and_then(KvShape::from_config)
            .map(Some)
            .map_err(|reason| format!("the draft model: {reason}")),
    }
}

/// The largest block-aligned context the grant holds for `shape` (and the
/// draft model's `draft`, whose KV shares the grant), capped at the model's
/// maximum position.
fn fitted_tokens(
    inputs: &FitInputs<'_>,
    shape: &KvShape,
    draft: Option<&KvShape>,
) -> Result<u64, String> {
    let mut per_token = shape
        .bytes_per_token(element_bytes(inputs, shape)?)
        .ok_or("the KV bytes per token overflow")?;
    if let Some(draft) = draft {
        // One KV dtype applies to both models' layers.
        let element = element_bytes(inputs, draft).map_err(|e| format!("the draft model: {e}"))?;
        per_token = draft
            .bytes_per_token(element)
            .and_then(|bytes| bytes.checked_add(per_token))
            .ok_or("the KV bytes per token overflow")?;
    }
    let grant = u64::try_from(inputs.kv_cache_bytes)
        .ok()
        .filter(|bytes| *bytes > 0)
        .ok_or("the KV cache grant is not positive")?;
    let block = u64::from(inputs.block_tokens.unwrap_or(DEFAULT_BLOCK_TOKENS).max(1));
    let blocks = (grant / per_token / block).saturating_sub(u64::from(inputs.reserved_blocks));
    let tokens = (blocks * block).min(shape.max_position);
    let aligned = tokens - tokens % block;
    if aligned == 0 {
        return Err(format!(
            "the KV cache grant of {grant} bytes holds fewer than one {block}-token block \
             at {per_token} bytes per token"
        ));
    }
    Ok(aligned)
}

/// ADR 0014 §5 (owner decision 2026-09-25): the effective context for a
/// launch, from the parsed model configuration (or why it could not be read).
pub fn fit_context(inputs: FitInputs<'_>, config: Result<&Value, String>) -> ContextFit {
    let config_value = config.as_ref().ok().copied();
    let shape = config.and_then(KvShape::from_config).map(|mut shape| {
        // Amendment A14: SGLang keeps a gated-delta-net hybrid's recurrent
        // state in a pool of its own, so only attention layers hold KV.
        if let Some(layers) = config_value
            .filter(|_| inputs.sglang)
            .and_then(sglang_pool::gated_delta_attention_layers)
        {
            shape.layers = layers;
            shape.approximation = None;
        }
        shape
    });
    let draft = draft_shape(&inputs);
    if let Some(declared) = inputs.declared {
        // An explicit context always wins. When the shape is exact and the
        // grant provably cannot hold it, say so; the engine still decides.
        let warning = match (&shape, &draft) {
            (Ok(shape), Ok(draft))
                if shape.approximation.is_none()
                    && draft.as_ref().is_none_or(|d| d.approximation.is_none()) =>
            {
                match (
                    fitted_tokens(&inputs, shape, draft.as_ref()),
                    u64::from(declared),
                ) {
                    (Ok(fits), declared_tokens)
                        if declared_tokens > fits && declared_tokens <= shape.max_position =>
                    {
                        Some(format!(
                            "engine_config.context_length {declared} exceeds the {fits} tokens \
                             the KV cache grant of {} bytes holds; the engine will refuse to \
                             start. Lower context_length or raise memory.kv_cache",
                            inputs.kv_cache_bytes
                        ))
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        return ContextFit {
            tokens: Some(declared),
            source: ContextSource::Declared,
            reason: None,
            warning,
            running_limit: None,
            streams: None,
        };
    }
    let shape = match shape {
        Ok(shape) => shape,
        Err(reason) => return fallback(reason),
    };
    let draft = match draft {
        Ok(draft) => draft,
        Err(reason) => return fallback(reason),
    };
    let reason = match &draft {
        None => shape.approximation.map(str::to_owned),
        Some(draft) => {
            let mut parts: Vec<String> =
                shape.approximation.map(str::to_owned).into_iter().collect();
            parts.push("the draft model's KV layers are counted with the checkpoint's".into());
            parts.extend(draft.approximation.map(|a| format!("draft model: {a}")));
            Some(parts.join("; "))
        }
    };
    // Owner decision 2026-10-02: a hybrid checkpoint on vLLM follows vLLM's
    // block layout rather than the per-token count.
    let hybrid = match (
        inputs.vllm,
        config_value,
        u64::try_from(inputs.kv_cache_bytes),
    ) {
        (Some(vllm), Some(config), Ok(grant)) if grant > 0 => element_bytes(&inputs, &shape)
            .ok()
            .and_then(|element| {
                vllm_hybrid::fitted_tokens(config, &shape, draft.as_ref(), element, grant, vllm)
            })
            .map(|fit| (fit, vllm.sequences)),
        _ => None,
    };
    if let Some((fit, sequences)) = hybrid {
        return match fit {
            Ok(tokens) => ContextFit {
                tokens: Some(u32::try_from(tokens).unwrap_or(u32::MAX)),
                source: ContextSource::Fitted,
                reason: Some(format!(
                    "hybrid model: fitted to vLLM's recurrent-state block layout for {sequences} \
                     sequences{}",
                    if draft.is_some() {
                        ", the draft model's layers included"
                    } else {
                        ""
                    }
                )),
                warning: None,
                running_limit: None,
                streams: None,
            },
            Err(reason) => fallback(reason),
        };
    }
    match fitted_tokens(&inputs, &shape, draft.as_ref()) {
        Ok(tokens) => ContextFit {
            tokens: Some(u32::try_from(tokens).unwrap_or(u32::MAX)),
            source: ContextSource::Fitted,
            reason,
            warning: None,
            running_limit: None,
            streams: None,
        },
        Err(reason) => fallback(reason),
    }
}

/// Read `<checkpoint>/config.json`, bounded, never following a symlink out of
/// a checkpoint that names none.
pub fn read_model_config(checkpoint_root: &Path) -> Result<Value, String> {
    use std::io::Read;
    let path = checkpoint_root.join("config.json");
    let file = std::fs::File::open(&path)
        .map_err(|_| "the checkpoint has no readable config.json".to_owned())?;
    let mut text = String::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|_| "the checkpoint's config.json is not readable text".to_owned())?;
    if text.len() as u64 > MAX_CONFIG_BYTES {
        return Err("the checkpoint's config.json is too large".into());
    }
    serde_json::from_str(&text).map_err(|_| "the checkpoint's config.json is not JSON".into())
}

/// The effective context of one launch of `settings` against the checkpoint
/// at `checkpoint_root`. `profile_args` are the installation's host-fixed
/// arguments: one that already sets the maximum model length wins, and capyctl
/// passes nothing (ADR 0014 §2, a typed field and a host-fixed argument never
/// both set one option).
pub fn fit_for_launch(
    engine: Engine,
    settings: &LaunchSettings,
    profile_args: &[String],
    checkpoint_root: Option<&Path>,
) -> ContextFit {
    let (common, memory, block, reserved_blocks) = match settings {
        LaunchSettings::Vllm(s) => (
            &s.common,
            &s.memory,
            s.block_size_tokens,
            VLLM_RESERVED_BLOCKS,
        ),
        LaunchSettings::Sglang(s) => (&s.common, &s.memory, None, 0),
        LaunchSettings::Tensorfold(s) => (&s.common, &s.memory, None, 0),
    };
    if common.context_length.is_none() {
        if let Some(option) = typed_field_option(engine, "context_length") {
            if option_names(profile_args).is_ok_and(|names| names.contains(option)) {
                return ContextFit {
                    tokens: None,
                    source: ContextSource::HostFixed,
                    reason: Some(format!("the installation's host-fixed args set `{option}`")),
                    warning: None,
                    running_limit: None,
                    streams: None,
                };
            }
        }
    }
    let config = checkpoint_root
        .ok_or_else(|| "the deployment resolves to no checkpoint directory".to_owned())
        .and_then(read_model_config);
    // The draft model is read where the checkpoint is: never by a server
    // fitting for a remote host. Resolution already confined its path to the
    // installation's approved directories (ADR 0014 §8).
    let args: Vec<String> = profile_args
        .iter()
        .chain(settings.extra_args())
        .cloned()
        .collect();
    let draft = match (engine, checkpoint_root) {
        (Engine::Vllm | Engine::Sglang, Some(_)) => {
            crate::engine_policy::draft_model_path(engine, &args)
                .map(|path| read_model_config(Path::new(&path)))
        }
        _ => None,
    };
    let vllm = (engine == Engine::Vllm).then(|| VllmFit {
        sequences: vllm_sequences(settings, profile_args),
        speculative_tokens: speculative_tokens(&args),
    });
    let mut fit = fit_context(
        FitInputs {
            declared: common.context_length,
            kv_cache_bytes: memory.kv_cache_bytes,
            kv_cache_dtype: common.kv_cache_dtype.as_deref(),
            dtype: common.dtype.as_deref(),
            block_tokens: block,
            reserved_blocks,
            draft: draft
                .as_ref()
                .map(|config| config.as_ref().map_err(String::as_str)),
            vllm,
            sglang: engine == Engine::Sglang,
        },
        config.as_ref().map_err(Clone::clone),
    );
    // Amendment A14: a hybrid model's recurrent state that does not fit the
    // memory request refuses the launch; status says so beforehand.
    // The pool is sized as the launch sizes it (`sglang_pool_for_launch`),
    // so status and a refused start name the same request.
    if let LaunchSettings::Sglang(sglang) = settings {
        match sglang_pool_for_launch(sglang, profile_args, checkpoint_root) {
            Ok(pool) => {
                fit.running_limit = pool.running_limit;
                // A derived request on a discrete device keeps the state in
                // the KV cache: a fitted context is held to the KV pool left.
                if let (ContextSource::Fitted, Some(tokens), Some(pool_tokens)) =
                    (fit.source, fit.tokens, pool.max_total_tokens)
                {
                    let block = DEFAULT_BLOCK_TOKENS;
                    let held = pool_tokens - pool_tokens % block;
                    if held < tokens && held > 0 {
                        fit.tokens = Some(held);
                        fit.reason = Some(match fit.reason.take() {
                            Some(reason) => {
                                format!("{reason}; held to the KV pool the recurrent state leaves")
                            }
                            None => "held to the KV pool the recurrent state leaves".into(),
                        });
                    }
                }
            }
            Err(refusal) => fit.warning = Some(refusal),
        }
    }
    fit.streams = tensorfold_streams(settings, profile_args, config.as_ref().ok());
    fit
}

/// Owner decision 2026-10-02: the `--max-num-seqs` CapyCTL passes vLLM when
/// neither the deployment (`max_concurrent_requests`) nor the installation's
/// host-fixed arguments set it: the router's per-deployment bound.
pub fn vllm_default_max_num_seqs(
    settings: &LaunchSettings,
    profile_args: &[String],
) -> Option<u32> {
    let declared = match settings {
        LaunchSettings::Vllm(s) => s.common.max_concurrent_requests,
        _ => return None,
    };
    let host_fixed = option_names(profile_args).is_ok_and(|names| names.contains("--max-num-seqs"));
    (declared.is_none() && !host_fixed)
        .then_some(capyctl_domain::launch::MAX_REQUESTS_PER_DEPLOYMENT)
}

/// ADR 0023 §4 (amended 2026-10-03, owner decision): the `--parallel` CapyCTL
/// passes TensorFold: the deployment's `max_concurrent_requests`, else
/// `TENSORFOLD_DEFAULT_PARALLEL`. `None` when the
/// extra or host-fixed arguments pass `--parallel` themselves (they win;
/// beside a declared count they are refused at resolution).
pub fn tensorfold_parallel(settings: &LaunchSettings, profile_args: &[String]) -> Option<u32> {
    let LaunchSettings::Tensorfold(s) = settings else {
        return None;
    };
    let passed = |args: &[String]| {
        crate::engine_policy::tensorfold_parallel(args).map_or(true, |found| found.is_some())
    };
    if passed(profile_args) || passed(&s.extra_args) {
        return None;
    }
    Some(
        s.common
            .max_concurrent_requests
            .unwrap_or(capyctl_domain::launch::TENSORFOLD_DEFAULT_PARALLEL),
    )
}

/// The model families whose CUDA engine TensorFold 0.6.3 to 0.6.5 runs one request at
/// a time whatever `--parallel` says (`families/nemotron_h`).
const TENSORFOLD_ONE_AT_A_TIME: &[&str] = &["nemotron_h"];

/// ADR 0023 §4 (amended 2026-10-03): the streams a TensorFold launch decodes
/// together, for status. `config` is the checkpoint's configuration when
/// this machine reads it.
fn tensorfold_streams(
    settings: &LaunchSettings,
    profile_args: &[String],
    config: Option<&Value>,
) -> Option<Streams> {
    let LaunchSettings::Tensorfold(s) = settings else {
        return None;
    };
    let mut streams = match tensorfold_parallel(settings, profile_args) {
        Some(count) => Streams {
            count: Some(count),
            source: if s.common.max_concurrent_requests.is_some() {
                StreamsSource::Declared
            } else {
                StreamsSource::Default
            },
            reason: None,
        },
        None => {
            let extra = crate::engine_policy::tensorfold_parallel(&s.extra_args)
                .ok()
                .flatten();
            let (value, source) = match extra {
                Some(value) => (value, StreamsSource::ExtraArgs),
                None => (
                    crate::engine_policy::tensorfold_parallel(profile_args)
                        .ok()
                        .flatten()
                        .flatten(),
                    StreamsSource::HostFixed,
                ),
            };
            // TensorFold's `auto` is one request at a time on CUDA.
            let count = value.and_then(|value| match value.trim().to_ascii_lowercase() {
                auto if auto == "auto" => Some(1),
                number => number
                    .parse::<i64>()
                    .ok()
                    .map(|n| n.clamp(1, i64::from(u32::MAX)) as u32),
            });
            Streams {
                count,
                source,
                reason: None,
            }
        }
    };
    let family = config.and_then(|config| config["model_type"].as_str());
    if let Some(family) = family.filter(|family| TENSORFOLD_ONE_AT_A_TIME.contains(family)) {
        if streams.count.is_some_and(|count| count > 1) {
            streams.count = Some(1);
            streams.reason = Some(format!(
                "TensorFold serves this model family ({family}) one request at a time on CUDA"
            ));
        }
    }
    Some(streams)
}

/// The sequences a vLLM launch runs with: declared, host-fixed, or the
/// default above. A host-fixed value that does not parse counts as vLLM's own
/// default (256).
fn vllm_sequences(settings: &LaunchSettings, profile_args: &[String]) -> u32 {
    if let LaunchSettings::Vllm(s) = settings {
        if let Some(declared) = s.common.max_concurrent_requests {
            return declared;
        }
    }
    if let Some(default) = vllm_default_max_num_seqs(settings, profile_args) {
        return default;
    }
    crate::engine_policy::parse_options(profile_args)
        .ok()
        .and_then(|options| {
            options
                .into_iter()
                .rev()
                .find(|o| o.name == "--max-num-seqs")
                .and_then(|o| o.value)
        })
        .and_then(|value| value.parse().ok())
        .unwrap_or(256)
}

/// `num_speculative_tokens` of vLLM's `--speculative-config`, when given.
fn speculative_tokens(args: &[String]) -> Option<u32> {
    crate::engine_policy::parse_options(args)
        .ok()?
        .into_iter()
        .rev()
        .find(|o| o.name == "--speculative-config")
        .and_then(|o| o.value)
        .and_then(|value| serde_json::from_str::<Value>(&value).ok())
        .map(|config| {
            config["num_speculative_tokens"]
                .as_u64()
                .map_or(1, |n| u32::try_from(n).unwrap_or(u32::MAX))
        })
}

/// The effective context of a resolved deployment on the machine that reads
/// its checkpoint.
pub fn fit_for_effective(effective: &crate::effective::EffectiveDeployment) -> ContextFit {
    fit_for_launch(
        effective.profile.engine,
        &sized_on_device(effective),
        &effective.profile.args,
        effective.model.resolved_path.as_deref().map(Path::new),
    )
}

/// SGLang's settings as a launch on a device domain sizes them: found live
/// 2026-10-04, `status` (which has observed no card total) sized a discrete
/// deployment's static pool as unified memory's and named another request
/// than the refused start. Only whether the device is discrete matters to the
/// pool, so the Ready allocation stands in for an unobserved total.
pub(crate) fn sized_on_device(effective: &crate::effective::EffectiveDeployment) -> LaunchSettings {
    let mut settings = effective.engine_config.clone();
    if let LaunchSettings::Sglang(sglang) = &mut settings {
        if sglang.memory.device_total_bytes.is_none() {
            if let Some((_, bytes)) = effective.ready_device_allocation() {
                sglang.memory.device_total_bytes = Some(bytes.max(1));
            }
        }
    }
    settings
}

/// The effective context as a server sees a deployment whose checkpoint is on
/// a remote host. A declared or host-fixed value is known here (with no fit
/// warning, which needs the checkpoint); an undeclared one is fitted by the
/// host at launch, and the server never reads a host's path locally.
pub fn fit_on_remote_host(effective: &crate::effective::EffectiveDeployment) -> ContextFit {
    let declared = match &effective.engine_config {
        LaunchSettings::Vllm(s) => s.common.context_length,
        LaunchSettings::Sglang(s) => s.common.context_length,
        LaunchSettings::Tensorfold(s) => s.common.context_length,
    };
    let fit = fit_for_launch(
        effective.profile.engine,
        &effective.engine_config,
        &effective.profile.args,
        None,
    );
    match (declared, fit.source) {
        (Some(_), _) | (None, ContextSource::HostFixed) => fit,
        _ => ContextFit {
            tokens: None,
            source: ContextSource::OnHost,
            reason: Some(
                "fitted to the KV cache grant by the host from its checkpoint at launch".into(),
            ),
            warning: None,
            running_limit: None,
            streams: tensorfold_streams(&effective.engine_config, &effective.profile.args, None),
        },
    }
}

#[cfg(test)]
mod tests;
