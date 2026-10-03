//! ADR 0014 amendment A14 (owner decision 2026-10-03): `memory.kv_cache` is
//! SGLang's KV pool, as it is vLLM's.
//!
//! SGLang sizes its pools from one static budget (`--mem-fraction-static`):
//! what the weights leave is split between the KV pool and, on a hybrid model,
//! the recurrent-state pool (`--mamba-full-memory-ratio`, about 0.9 to 1).
//! Found live on 2026-10-03 with SGLang 0.5.21, Qwen3.8-27B NVFP4 and DFlash2:
//! a 16 GiB `kv_cache` gave a 211k-token pool where vLLM held 322k, and the
//! state pool capped the running requests at 2.
//!
//! CapyCTL now states both pools in tokens and slots, read from the
//! checkpoint where it is read for the context fit:
//!
//! - the KV pool is `kv_cache` divided by SGLang's KV bytes per token
//!   (full-attention layers only on a gated-delta-net hybrid, plus the draft
//!   model's KV layers), passed as `--max-total-tokens`;
//! - on a gated-delta-net hybrid, the state pool holds
//!   [`STATE_SLOTS_PER_REQUEST`] slots per running request
//!   (`--max-mamba-cache-size`), and the running requests are the deployment's
//!   `max_concurrent_requests`, or the most of CapyCTL's in-flight bound
//!   (`MAX_REQUESTS_PER_DEPLOYMENT`) whose state fits;
//! - the static pool (memory request less the margin) must hold the weights,
//!   the KV cache and that state. With an explicit request, a declared
//!   `max_concurrent_requests` whose state does not fit, or one running
//!   request that does not, is refused with the memory request that would
//!   hold it. A derived request (owner decision 2026-10-03) fits what it can:
//!   the state may take up to half of the margin on unified memory, the
//!   running requests are limited to what fits, and only one request that
//!   does not fit is refused.
//!
//! SGLang 0.5.20 and 0.5.21 (`kv_cache_configurator.py`): an explicit
//! `max_mamba_cache_size` fixes the state pool, reserving
//! `(slots + 1) × state` plus, with speculative decoding, `(running + 1) ×
//! draft tokens × state` of intermediate states; `max_total_tokens` caps the
//! KV pool at what is left. Shapes this module does not model (sliding-window
//! or latent attention, other hybrids) keep SGLang's own sizing, with the
//! reason recorded.

use std::path::Path;

use capyctl_domain::launch::{MemoryRequest, SglangLaunchSettings, MAX_REQUESTS_PER_DEPLOYMENT};
use serde_json::Value;

use super::{dtype_bytes, positive, read_model_config, KvShape};
use crate::engine_policy::{matches_name, parse_options, Engine};

/// State slots SGLang 0.5.20 and 0.5.21 keep per running request on a
/// hybrid model with the radix cache, overlap scheduling and the extra
/// state buffer on (their defaults): 3 plus 2 ping-pong slots. Every other
/// mode keeps fewer, so this is an upper bound. Found live on 2026-10-03:
/// "max_mamba_cache_size=14, 5 state slots per request".
pub const STATE_SLOTS_PER_REQUEST: u64 = 5;

/// What CapyCTL tells SGLang about its pools; `None` leaves a setting as the
/// deployment (or the engine) has it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SglangPool {
    /// `--max-total-tokens`: the KV pool in tokens.
    pub max_total_tokens: Option<u32>,
    /// `--max-running-requests`, when CapyCTL sized the state pool for it.
    pub max_running_requests: Option<u32>,
    /// `--max-mamba-cache-size`: the recurrent-state pool in slots.
    pub max_mamba_cache_size: Option<u32>,
    /// The running requests, when the state that fits holds fewer than the
    /// deployment's count (or CapyCTL's in-flight bound).
    pub running_limit: Option<u32>,
    /// Bytes of the margin a derived request's state takes; the static pool
    /// grows by them.
    pub borrowed_margin_bytes: u64,
    /// Why a pool is left to SGLang, or how it was sized.
    pub reason: Option<String>,
}

/// ADR 0014 §5: SGLang's static pool and the margin left outside it. A
/// discrete device's margin is a tenth of the weights share (discrete GPU
/// design §3, §6); unified memory keeps the family margin. An explicit request
/// smaller than KV plus margin (a declared `resources:` Ready phase) gives the
/// static pool the declared KV cache, and never more than the whole request.
pub fn static_pool_bytes(memory: &MemoryRequest) -> (i64, i64) {
    let margin = match memory.device_total_bytes {
        Some(_) => (memory.request_bytes - memory.kv_cache_bytes) / 11,
        None => memory.margin_bytes,
    };
    let static_bytes = (memory.request_bytes - margin)
        .max(memory.kv_cache_bytes)
        .min(memory.request_bytes);
    (margin, static_bytes)
}

/// A gated-delta-net hybrid as SGLang lays it out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GatedDeltaHybrid {
    attention_layers: u64,
    /// One request's recurrent state across every linear-attention layer.
    state_bytes: u64,
}

fn text_config(config: &Value) -> &Value {
    match config.get("text_config") {
        Some(text) if text.is_object() && config.get("num_hidden_layers").is_none() => text,
        _ => config,
    }
}

/// The full-attention layer count of a gated-delta-net hybrid
/// (`layer_types` of `full_attention` and `linear_attention` only).
pub(super) fn gated_delta_attention_layers(config: &Value) -> Option<u64> {
    let types = text_config(config).get("layer_types")?.as_array()?;
    let count = |kind: &str| types.iter().filter(|t| t.as_str() == Some(kind)).count() as u64;
    let (attention, recurrent) = (count("full_attention"), count("linear_attention"));
    (attention > 0 && recurrent > 0 && attention + recurrent == types.len() as u64)
        .then_some(attention)
}

/// SGLang's state per request (`Mamba2CacheParams.mamba_cache_per_req`):
/// per linear-attention layer, a bfloat16 convolution state of
/// `kernel - 1` positions and a temporal state in `mamba_ssm_dtype`
/// (float32 unless the configuration or `--mamba-ssm-dtype` says otherwise).
fn gated_delta_hybrid(config: &Value, ssm_dtype: Option<&str>) -> Option<GatedDeltaHybrid> {
    let attention_layers = gated_delta_attention_layers(config)?;
    let text = text_config(config);
    let types = text.get("layer_types")?.as_array()?;
    let recurrent = types.len() as u64 - attention_layers;
    let key_heads = positive(text, "linear_num_key_heads")?;
    let key_dim = positive(text, "linear_key_head_dim")?;
    let value_heads = positive(text, "linear_num_value_heads")?;
    let value_dim = positive(text, "linear_value_head_dim")?;
    let kernel = positive(text, "linear_conv_kernel_dim")?;
    let ssm = match ssm_dtype.or_else(|| text.get("mamba_ssm_dtype").and_then(Value::as_str)) {
        Some(name) => dtype_bytes(name)?,
        None => 4,
    };
    let conv_dim = key_heads * key_dim * 2 + value_heads * value_dim;
    let conv = conv_dim
        .checked_mul(kernel.checked_sub(1)?)?
        .checked_mul(2)?;
    let temporal = value_heads
        .checked_mul(key_dim)?
        .checked_mul(value_dim)?
        .checked_mul(ssm)?;
    Some(GatedDeltaHybrid {
        attention_layers,
        state_bytes: recurrent.checked_mul(conv.checked_add(temporal)?)?,
    })
}

/// The last value the arguments give `option` (abbreviations included), and
/// whether they name it at all.
fn option_value(args: &[String], option: &str) -> (bool, Option<String>) {
    let Ok(options) = parse_options(args) else {
        return (false, None);
    };
    let mut named = false;
    let mut value = None;
    for parsed in options {
        if matches_name(&parsed.name, option) {
            named = true;
            value = parsed.value;
        }
    }
    (named, value)
}

/// The width of one cached KV element on SGLang: the KV dtype, else the
/// deployment's dtype, else the checkpoint's.
fn kv_element(settings: &SglangLaunchSettings, shape: &KvShape) -> Result<u64, String> {
    let common = &settings.common;
    for (name, value) in [
        ("KV cache dtype", &common.kv_cache_dtype),
        ("dtype", &common.dtype),
    ] {
        if let Some(value) = value.as_deref().filter(|v| !v.eq_ignore_ascii_case("auto")) {
            return dtype_bytes(value).ok_or_else(|| format!("unknown {name} `{value}`"));
        }
    }
    shape
        .model_dtype_bytes
        .ok_or_else(|| "the model configuration names no dtype".into())
}

fn left_to_sglang(reason: impl Into<String>) -> SglangPool {
    SglangPool {
        reason: Some(format!("{}; SGLang sizes its pools itself", reason.into())),
        ..SglangPool::default()
    }
}

/// The recurrent state SGLang reserves for `running` requests of `state`
/// bytes each: the slots plus a padding slot, and with speculative decoding
/// one intermediate state per draft token for each running request plus one.
fn state_bytes(running: u64, state: u64, draft_tokens: u64) -> Option<u64> {
    let slots = running
        .checked_mul(STATE_SLOTS_PER_REQUEST)?
        .checked_add(1)?;
    let intermediate = if draft_tokens > 0 {
        (running + 1).checked_mul(draft_tokens)?
    } else {
        0
    };
    slots.checked_add(intermediate)?.checked_mul(state)
}

/// SGLang's pools for `settings`, from the checkpoint configuration (and the
/// draft model's) at `checkpoint_root`. `Err` is a refusal: the state the
/// running requests need does not fit the memory request.
pub fn sglang_pool(
    settings: &SglangLaunchSettings,
    profile_args: &[String],
    config: Result<&Value, String>,
    draft: Option<Result<&Value, &str>>,
) -> Result<SglangPool, String> {
    let config = match config {
        Ok(config) => config,
        Err(reason) => return Ok(left_to_sglang(reason)),
    };
    let args: Vec<String> = profile_args
        .iter()
        .chain(&settings.extra_args)
        .cloned()
        .collect();
    let shape = match KvShape::from_config(config) {
        Ok(shape) => shape,
        Err(reason) => return Ok(left_to_sglang(reason)),
    };
    let element = match kv_element(settings, &shape) {
        Ok(element) => element,
        Err(reason) => return Ok(left_to_sglang(reason)),
    };
    let (_, ssm_dtype) = option_value(&args, "--mamba-ssm-dtype");
    let hybrid = gated_delta_hybrid(config, ssm_dtype.as_deref());
    let target = match (hybrid, shape.approximation) {
        (Some(hybrid), _) => {
            2 * shape.kv_heads * shape.head_dim * element * hybrid.attention_layers
        }
        (None, None) => match shape.bytes_per_token(element) {
            Some(bytes) => bytes,
            None => return Ok(left_to_sglang("the KV bytes per token overflow")),
        },
        (None, Some(approximation)) => return Ok(left_to_sglang(approximation)),
    };
    // Amendment A9: the draft model's KV layers share the pool. SGLang counts
    // every draft layer as full attention (`dflash_draft_cell_size_per_token`)
    // in the deployment's KV dtype, or `--speculative-draft-kv-cache-dtype`.
    let draft_bytes = match draft {
        None => 0,
        Some(Err(reason)) => return Ok(left_to_sglang(format!("the draft model: {reason}"))),
        Some(Ok(draft)) => {
            let draft = match KvShape::from_config(draft) {
                Ok(draft) => draft,
                Err(reason) => return Ok(left_to_sglang(format!("the draft model: {reason}"))),
            };
            let element = match option_value(&args, "--speculative-draft-kv-cache-dtype").1 {
                Some(name) if !name.eq_ignore_ascii_case("auto") => dtype_bytes(&name)
                    .ok_or_else(|| format!("unknown draft KV cache dtype `{name}`")),
                _ => kv_element(settings, &draft),
            };
            match element.map(|element| draft.bytes_per_token(element)) {
                Ok(Some(bytes)) => bytes,
                Ok(None) => return Ok(left_to_sglang("the draft model's KV bytes overflow")),
                Err(reason) => return Ok(left_to_sglang(format!("the draft model: {reason}"))),
            }
        }
    };
    let per_token = target.saturating_add(draft_bytes);
    let memory = &settings.memory;
    let kv = u64::try_from(memory.kv_cache_bytes).unwrap_or(0);
    let mut pool = SglangPool::default();
    // A declared `max_total_tokens` is the deployment's own KV pool.
    if settings.max_total_tokens.is_none() {
        let tokens = kv / per_token.max(1);
        if tokens == 0 {
            return Ok(left_to_sglang(format!(
                "the KV cache of {kv} bytes holds no token at {per_token} bytes per token"
            )));
        }
        pool.max_total_tokens = Some(u32::try_from(tokens.min(i32::MAX as u64)).unwrap_or(0));
    }
    let Some(hybrid) = hybrid else {
        return Ok(pool);
    };
    // The deployment's (or the installation's) arguments own the state pool
    // when they size it; CapyCTL then passes nothing for it.
    for option in ["--max-mamba-cache-size", "--mamba-full-memory-ratio"] {
        if option_value(&args, option).0 {
            pool.reason = Some(format!("the arguments set `{option}`"));
            return Ok(pool);
        }
    }
    let draft_tokens = if option_value(&args, "--speculative-algorithm").0 {
        match option_value(&args, "--speculative-num-draft-tokens")
            .1
            .and_then(|value| value.parse::<u64>().ok())
        {
            Some(tokens) => tokens,
            None => {
                pool.reason = Some(
                    "speculative decoding without `--speculative-num-draft-tokens`: SGLang \
                     sizes the recurrent-state pool itself"
                        .into(),
                );
                return Ok(pool);
            }
        }
    } else {
        0
    };
    let declared = settings.common.max_concurrent_requests;
    let Some(weights) = memory.weights_bytes.and_then(|w| u64::try_from(w).ok()) else {
        // Without the weights the room is unknown: size the state for the
        // declared requests only, and let SGLang check it.
        if let Some(running) = declared {
            pool.max_mamba_cache_size = state_slots(running);
        }
        pool.reason = Some("the checkpoint's weight size is not known".into());
        return Ok(pool);
    };
    let (margin, static_bytes) = static_pool_bytes(memory);
    let room = u64::try_from(static_bytes)
        .unwrap_or(0)
        .saturating_sub(weights.saturating_add(kv));
    let need = |running: u32| state_bytes(u64::from(running), hybrid.state_bytes, draft_tokens);
    // Owner decision 2026-10-03: a request CapyCTL derived (weights + KV +
    // margin, nothing for the state) fits what it can, the state taking up to
    // half of the margin on unified memory; the other half stays for SGLang's
    // runtime outside its static pool. An explicit request is strict.
    let derived = derived_request(settings, weights);
    let borrowable = if derived && memory.device_total_bytes.is_none() {
        u64::try_from(margin / 2).unwrap_or(0)
    } else {
        0
    };
    let fits = |running: u32| need(running).is_some_and(|bytes| bytes <= room + borrowable);
    let bound = declared.unwrap_or(MAX_REQUESTS_PER_DEPLOYMENT);
    let largest = (1..=bound).rev().find(|running| fits(*running));
    let running = match (largest, declared) {
        (Some(running), Some(declared)) if running == declared || derived => running,
        (Some(running), None) => running,
        _ => {
            // An explicit request names the declared count; otherwise the
            // one request that does not fit.
            let running = match declared {
                Some(declared) if !derived => declared,
                _ => 1,
            };
            let needed = need(running).unwrap_or(u64::MAX);
            let short = needed.saturating_sub(room + borrowable);
            return Err(format!(
                "SGLang keeps {needed} bytes of recurrent state for {running} running \
                 request{} of this hybrid model ({} state slots of {} bytes{}), beside {weights} \
                 bytes of weights and the {kv}-byte KV cache, and the memory request leaves \
                 {} bytes for it; raise engine_config.memory.request by {short} bytes (to \
                 {}){}",
                if running == 1 { "" } else { "s" },
                u64::from(running) * STATE_SLOTS_PER_REQUEST + 1,
                hybrid.state_bytes,
                if draft_tokens > 0 {
                    format!(
                        " and {} draft-token states",
                        (u64::from(running) + 1) * draft_tokens
                    )
                } else {
                    String::new()
                },
                room + borrowable,
                u64::try_from(memory.request_bytes)
                    .unwrap_or(0)
                    .saturating_add(short),
                if running > 1 {
                    " or lower max_concurrent_requests"
                } else {
                    ""
                },
            ));
        }
    };
    pool.max_mamba_cache_size = state_slots(running);
    if declared != Some(running) {
        pool.max_running_requests = Some(running);
    }
    if running < bound {
        pool.running_limit = Some(running);
    }
    pool.borrowed_margin_bytes = need(running)
        .unwrap_or(0)
        .saturating_sub(room)
        .min(borrowable);
    pool.reason = Some(format!(
        "hybrid model: recurrent state for {running} running request{} sized beside the KV cache",
        if running == 1 { "" } else { "s" }
    ));
    Ok(pool)
}

/// The request was derived from the weights (weights + KV + margin), so it
/// holds nothing for a hybrid model's state.
fn derived_request(settings: &SglangLaunchSettings, weights: u64) -> bool {
    let memory = &settings.memory;
    settings.provenance.get("memory.request")
        == Some(&capyctl_domain::launch::SettingSource::Derived)
        && u64::try_from(memory.request_bytes).ok()
            == weights
                .checked_add(u64::try_from(memory.kv_cache_bytes).unwrap_or(0))
                .and_then(|sum| sum.checked_add(u64::try_from(memory.margin_bytes).ok()?))
}

fn state_slots(running: u32) -> Option<u32> {
    u32::try_from(u64::from(running) * STATE_SLOTS_PER_REQUEST).ok()
}

/// [`sglang_pool`] for a launch, reading the checkpoint (and the draft
/// model) where the launch reads them.
pub fn sglang_pool_for_launch(
    settings: &SglangLaunchSettings,
    profile_args: &[String],
    checkpoint_root: Option<&Path>,
) -> Result<SglangPool, String> {
    let config = checkpoint_root
        .ok_or_else(|| "the deployment resolves to no checkpoint directory".to_owned())
        .and_then(read_model_config);
    let args: Vec<String> = profile_args
        .iter()
        .chain(&settings.extra_args)
        .cloned()
        .collect();
    let draft_path =
        checkpoint_root.and_then(|_| crate::engine_policy::draft_model_path(Engine::Sglang, &args));
    let draft = draft_path
        .as_ref()
        .map(|path| read_model_config(Path::new(path)));
    // A revision that declares its request and KV cache is not re-resolved
    // with the measured weights (found live 2026-10-03), so the launch sizes
    // them here, where it reads the checkpoint: the weight files of the
    // checkpoint and of the draft model (amendment A6).
    let mut measured;
    let settings = match (settings.memory.weights_bytes, checkpoint_root) {
        (None, Some(root)) => {
            measured = settings.clone();
            measured.memory.weights_bytes = [Some(root), draft_path.as_deref().map(Path::new)]
                .into_iter()
                .flatten()
                .map(weight_file_bytes)
                .sum::<Option<u64>>()
                .and_then(|bytes| i64::try_from(bytes).ok())
                .filter(|bytes| *bytes > 0);
            &measured
        }
        _ => settings,
    };
    sglang_pool(
        settings,
        profile_args,
        config.as_ref().map_err(Clone::clone),
        draft
            .as_ref()
            .map(|config| config.as_ref().map_err(String::as_str)),
    )
}

/// The weight files a checkpoint directory loads (`.safetensors`, `.bin`,
/// `.pt`, `.pth`, `.gguf`, as the checkpoint measurement counts them), two
/// directory levels deep; `None` when the directory cannot be read.
fn weight_file_bytes(root: &Path) -> Option<u64> {
    fn walk(dir: &Path, depth: u8) -> Option<u64> {
        let mut total = 0u64;
        for entry in std::fs::read_dir(dir).ok()? {
            let path = entry.ok()?.path();
            let metadata = std::fs::metadata(&path).ok()?;
            if metadata.is_dir() {
                if depth > 0 {
                    total = total.checked_add(walk(&path, depth - 1)?)?;
                }
            } else if path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| matches!(e, "safetensors" | "bin" | "pt" | "pth" | "gguf"))
            {
                total = total.checked_add(metadata.len())?;
            }
        }
        Some(total)
    }
    walk(root, 1)
}

/// [`sglang_pool_for_launch`] for a resolved deployment on the machine that
/// reads its checkpoint; `None` for another engine.
pub fn sglang_pool_for_effective(
    effective: &crate::effective::EffectiveDeployment,
) -> Option<Result<SglangPool, String>> {
    match &effective.engine_config {
        capyctl_domain::launch::LaunchSettings::Sglang(settings) => Some(sglang_pool_for_launch(
            settings,
            &effective.profile.args,
            effective.model.resolved_path.as_deref().map(Path::new),
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
