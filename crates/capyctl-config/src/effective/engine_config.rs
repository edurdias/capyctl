//! ADR 0014: resolution of a deployment's `engine_config` into launch settings.
//!
//! Owner decisions E1 and P2: every engine serves any model; the deployment
//! chooses its parameters. capyctl validates type, range and ownership here, not
//! whether the engine supports a value on this checkpoint (ADR 0011).

use super::*;
use crate::engine_policy::{
    option_names, typed_field_option, validate_extra_args, ExtraArgsContext, ExtraArgsPolicy,
};
use capyctl_domain::launch::{
    CommonEngineSettings, LaunchSettings, MemoryRequest, SafetensorsLoadStrategy, SettingSource,
    SglangLaunchSettings, TensorfoldLaunchSettings, VllmLaunchSettings,
};

/// ADR 0014 §5: conservative placeholder overhead margins, per engine family,
/// until M16 measures peak minus weights minus KV for each model and engine.
pub const VLLM_OVERHEAD_MARGIN_BYTES: i64 = 8 << 30;
pub const SGLANG_OVERHEAD_MARGIN_BYTES: i64 = 8 << 30;
// ADR 0023 §4: a TensorFold deployment declares its resources, so nothing is derived from a margin.
pub const TENSORFOLD_OVERHEAD_MARGIN_BYTES: i64 = 0;

/// ADR 0014 §5: the parked phase is the engine's residual floor, also to be
/// measured. Until then a parking deployment reserves this placeholder (or its
/// whole request, when smaller) while parked.
pub const PARKED_RESIDUAL_PLACEHOLDER_BYTES: i64 = 2 << 30;

/// Discrete GPU design §3 (ADR 0019): the host RAM an engine process holds outside
/// the GPU (interpreter, CUDA runtime, tokenizer, pinned staging buffers), charged
/// on the system domain of a discrete host. A placeholder until a first run
/// measures the process RSS.
pub const ENGINE_HOST_OVERHEAD_PLACEHOLDER_BYTES: i64 = 4 << 30;

/// Discrete GPU design §3: what a parked engine still holds on a discrete GPU
/// (CUDA context, NCCL and allocator buffers). A placeholder until measured;
/// `PARKED_RESIDUAL_PLACEHOLDER_BYTES` stays for unified hosts.
pub const PARKED_DEVICE_RESIDUE_PLACEHOLDER_BYTES: i64 = 1 << 30;

/// ADR 0019 (discrete GPU design §3): the device memory an engine holds beyond
/// its memory request (the CUDA context and the CUDA graphs it captures after
/// sizing its KV cache), charged on the device domain, or the unified pool, in
/// every active phase: one rule on every host shape.
/// Measured live on a 16 GB discrete GPU with vLLM 0.29: a 12.0 GiB request
/// held 13.2 GiB of the card, 1.2 GiB beyond it. SGLang's static memory
/// fraction likewise leaves its graphs outside it, so one rule charges both
/// engines. A placeholder (1.25 GiB) until the measured device peak replaces
/// it.
pub const ENGINE_DEVICE_OVERHEAD_PLACEHOLDER_BYTES: i64 = 5 << 28;

/// Discrete GPU design §3: the host RAM a `host_backed` copy of `weights`
/// bytes takes. The copy is pinned host memory, which PyTorch's pinned
/// allocator rounds up per tensor. Measured live on a 16 GB discrete GPU with
/// vLLM 0.29: about 1.37 times the weights (11.1 GB for 8.04 GB of Qwen3-4B,
/// 4.2 GB for 3.09 GB of Qwen2.5-1.5B), so the placeholder charges 1.5 times
/// until a first park measures it.
pub const HOST_BACKED_COPY_FACTOR: (i64, i64) = (3, 2);

/// The `host_backed` copy of `weights` bytes, as charged in host RAM.
pub fn host_backed_copy_bytes(weights: i64) -> i64 {
    let (numerator, denominator) = HOST_BACKED_COPY_FACTOR;
    weights.saturating_mul(numerator) / denominator
}

/// Owner decision 2026-09-23 (startup memory budget): until a deployment
/// declares `memory.startup` or a first run on a host measures the peak, the
/// startup reservation is `max(request, weights × STARTUP_WEIGHTS_FACTOR +
/// margin)`. A conservative placeholder, not a measurement: loading reads the
/// weights through transient buffers, and M16 saw JIT compilation at startup
/// drop MemAvailable far below the steady footprint. Numerator and denominator
/// of the factor: 2.25 since ADR 0014 amendment A8 (found live 2026-10-02:
/// vLLM 0.30 loading Qwen3.8-27B NVFP4, 20.42 GiB of weights, at a 4 GiB KV
/// cache dropped MemAvailable by up to 50.49 GiB, 2.08 × weights + 8 GiB,
/// against the 41.92 GiB the 1.6 factor reserved).
pub const STARTUP_WEIGHTS_FACTOR: (i64, i64) = (9, 4);

/// The factor of a revision frozen before amendment A8 (1.6), which it keeps.
pub const LEGACY_STARTUP_WEIGHTS_FACTOR: (i64, i64) = (8, 5);

/// ADR 0014 amendment A8 (found live 2026-10-02): the first-start graph
/// allowance, per model whose CUDA graphs the engine captures (the checkpoint,
/// and the draft model of a speculative deployment). Graphs are captured after
/// the KV cache is allocated, so the allowance stacks on the request. vLLM 0.30
/// with Qwen3.8-27B NVFP4 and DFlash2 captured 1.64 GiB of graphs against the
/// 1.25 GiB `ENGINE_DEVICE_OVERHEAD_PLACEHOLDER_BYTES` and its first start
/// peaked 1.21 GiB above a cold phase without this allowance. A placeholder
/// until a first run measures the peak, which then replaces it.
pub const STARTUP_GRAPH_ALLOWANCE_BYTES: i64 = 5 << 28;

/// ADR 0014 amendment A8: the graph allowance a derived startup placeholder
/// carries: one `STARTUP_GRAPH_ALLOWANCE_BYTES` per captured model for vLLM
/// and SGLang, nothing for TensorFold (which declares its resources).
pub fn startup_graph_allowance(engine: Engine, draft_model: bool) -> i64 {
    match engine {
        Engine::Vllm | Engine::Sglang => {
            STARTUP_GRAPH_ALLOWANCE_BYTES * if draft_model { 2 } else { 1 }
        }
        Engine::Tensorfold => 0,
    }
}

/// Owner decision 2026-09-23: the placeholder startup peak for a request,
/// the checkpoint's weights (when known), the family margin and (amendment
/// A8) the graph allowance: `max(request + graphs, weights × 2.25 + margin)`.
/// `graphs` is `None` for a revision frozen before amendment A8, which keeps
/// `max(request, weights × 1.6 + margin)`.
pub fn default_startup_bytes(
    request: i64,
    weights: Option<i64>,
    margin: i64,
    graphs: Option<i64>,
) -> Option<i64> {
    let floor = request.checked_add(graphs.unwrap_or(0))?;
    let Some(weights) = weights else {
        return Some(floor);
    };
    let (numerator, denominator) = if graphs.is_some() {
        STARTUP_WEIGHTS_FACTOR
    } else {
        LEGACY_STARTUP_WEIGHTS_FACTOR
    };
    weights
        .checked_mul(numerator)
        .map(|scaled| scaled / denominator)
        .and_then(|scaled| scaled.checked_add(margin))
        .map(|peak| peak.max(floor))
}

/// The per-family overhead margin: the floor of [`unified_margin`], and the
/// margin of the startup placeholder ([`default_startup_bytes`]).
pub fn overhead_margin(engine: Engine) -> i64 {
    match engine {
        Engine::Vllm => VLLM_OVERHEAD_MARGIN_BYTES,
        Engine::Sglang => SGLANG_OVERHEAD_MARGIN_BYTES,
        Engine::Tensorfold => TENSORFOLD_OVERHEAD_MARGIN_BYTES,
    }
}

/// ADR 0014 amendment A18 (owner decision 2026-10-07): the percent of the
/// weights the margin on unified memory carries beside the engine's
/// CPU-side memory. Found live on GB10 with vLLM 0.30.0 and gpt-oss-120b
/// (60.77 GiB of weights, an 8 GiB KV cache): the engine held 7.3 GiB of
/// GPU memory beyond the weights and the KV cache (12 % of the weights)
/// and 5.2 GiB on the CPU side; the 8 GiB family margin left the Ready
/// charge 3.3 GiB short of the 81.3 GiB in use.
pub const UNIFIED_MARGIN_WEIGHTS_PERCENT: i64 = 15;

/// ADR 0014 amendment A18: the CPU-side term of the margin on unified
/// memory, the host RAM a discrete host is charged for the same engine
/// process ([`ENGINE_HOST_OVERHEAD_PLACEHOLDER_BYTES`], discrete GPU design
/// §3). On unified memory both come out of the one pool the request holds.
pub const UNIFIED_MARGIN_HOST_BYTES: i64 = ENGINE_HOST_OVERHEAD_PLACEHOLDER_BYTES;

/// ADR 0014 amendment A18: the margin a memory request on unified memory
/// holds beside the weights and the KV cache, declared or derived:
/// `max(family margin, weights x 0.15 + 4 GiB)`. The family margin (8 GiB)
/// is what the GB10 recipes measured for checkpoints up to 22 GiB and stays
/// the floor, so the term only grows the margin above 26.7 GiB of weights.
/// Unknown weights keep the floor; TensorFold, which declares its
/// resources, keeps none.
pub fn unified_margin(engine: Engine, weights: Option<i64>) -> i64 {
    let floor = overhead_margin(engine);
    match weights {
        Some(weights) if floor > 0 => (weights / 100)
            .saturating_mul(UNIFIED_MARGIN_WEIGHTS_PERCENT)
            .saturating_add(UNIFIED_MARGIN_HOST_BYTES)
            .max(floor),
        _ => floor,
    }
}

/// ADR 0014 §2: the dtypes capyctl accepts by name; anything else is refused.
const DTYPES: &[&str] = &["auto", "bfloat16", "float16", "float32"];

/// What the checkpoint manifest (ADR 0014 §7, WE3) tells resolution. Absent
/// facts never block a deployment that declares its memory in full.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CheckpointFacts {
    /// Sum of the checkpoint's weight-file sizes.
    pub weights_bytes: Option<i64>,
    /// ADR 0014 amendment A16: one request slot of a hybrid model's recurrent
    /// state on SGLang, measured by the host from `config.json`
    /// (`context_fit::sglang_state_slot_bytes`). `None` for any other model
    /// and in a revision measured before the fact existed, which keeps its
    /// sizing.
    pub state_slot_bytes: Option<i64>,
    /// A snapshot frozen before the startup memory budget existed records no
    /// startup peak; it is re-resolved exactly as it was, with a cold phase
    /// equal to the request. Never set for a new resolution.
    pub legacy_startup: bool,
    /// A snapshot frozen before the engine's CUDA context and graphs were
    /// charged records no `overhead_bytes`; it re-resolves exactly as it was,
    /// without them. Never set for a new resolution.
    pub legacy_overhead: bool,
    /// ADR 0014 amendment A8: a snapshot frozen before the first-start graph
    /// allowance records no `startup_graphs_bytes`; its placeholder startup
    /// re-resolves exactly as it was, without it. Never set for a new
    /// resolution.
    pub legacy_startup_graphs: bool,
    /// A snapshot that records the engine family's margin (8 GiB) re-resolves
    /// with it, exactly as it was: beside a request declared for a discrete
    /// GPU it was sized before the device margin (weights x 0.10, ADR 0019 §3,
    /// 2026-10-03), and on unified memory before the margin grew with the
    /// weights (ADR 0014 amendment A18, 2026-10-07). Never set for a new
    /// resolution.
    pub legacy_family_margin: bool,
    /// ADR 0014 amendment A13: a snapshot frozen while CapyCTL turned SGLang's
    /// CUDA graphs off beside the memory saver records that default; it
    /// re-resolves exactly as it was. Never set for a new resolution.
    pub legacy_sglang_graphs_off: bool,
    /// ADR 0028 §5 (amendment of 2026-10-07): how the checkpoint's weights
    /// split across ranks, read by the host from its safetensors headers
    /// beside the weights. Used only by a group member whose phases derive
    /// from `engine_config.memory`; `None` takes the fallback allowance.
    pub layout: Option<capyctl_domain::member_weights::CheckpointLayout>,
    /// ADR 0028 §5: the topology of the group a member snapshot was resolved
    /// for (its `memory.member`), which the document rebuilt from the
    /// snapshot no longer states. Never set for a resolution from source.
    pub member_of: Option<crate::topology::Topology>,
    /// ADR 0014 amendment A20 (owner decision 2026-10-09): the tables the
    /// host found in the checkpoint's safetensors headers. Used only when the
    /// engine's arguments keep them on disk
    /// (`engine_policy::disk_table_cache_bytes`).
    pub disk_tables: Option<capyctl_domain::disk_tables::CheckpointTables>,
}

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawEngineConfig {
    #[serde(default)]
    dtype: Option<String>,
    #[serde(default)]
    quantization: Option<String>,
    #[serde(default)]
    kv_cache_dtype: Option<String>,
    #[serde(default)]
    context_length: Option<u32>,
    #[serde(default)]
    max_concurrent_requests: Option<u32>,
    #[serde(default)]
    cuda_graphs: Option<bool>,
    #[serde(default)]
    language_model_only: Option<bool>,
    #[serde(default)]
    trust_remote_code: Option<bool>,
    #[serde(default)]
    memory: Option<RawMemory>,
    #[serde(default)]
    vllm: Option<RawVllmFields>,
    #[serde(default)]
    sglang: Option<RawSglangFields>,
    #[serde(default)]
    tensorfold: Option<RawTensorfoldFields>,
    #[serde(default)]
    accept_extra_args: Option<bool>,
    #[serde(default)]
    extra_args: Option<Vec<String>>,
    // ADR 0028 §2.1: the deployment's engine environment.
    #[serde(default)]
    env: BTreeMap<String, String>,
}

impl RawEngineConfig {
    /// The engine environment as written (resolved against the profile later).
    pub(super) fn env(&self) -> &BTreeMap<String, String> {
        &self.env
    }

    /// The extra engine arguments as written (validated later).
    pub(super) fn extra_args(&self) -> &[String] {
        self.extra_args.as_deref().unwrap_or_default()
    }

    /// Whether the block states a memory request or a KV cache.
    pub(super) fn states_memory(&self) -> bool {
        self.memory
            .as_ref()
            .is_some_and(|memory| memory.request.is_some() || memory.kv_cache.is_some())
    }

    /// Owner decision 2026-09-25: state the default KV cache, in bytes, for a
    /// deployment that states no memory (`deployment_defaults::default_kv_cache`).
    pub(super) fn default_kv_cache(&mut self, bytes: i64) {
        self.memory.get_or_insert_with(RawMemory::default).kv_cache = Some(format!("{bytes}B"));
    }
}

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMemory {
    #[serde(default)]
    request: Option<String>,
    #[serde(default)]
    kv_cache: Option<String>,
    #[serde(default)]
    startup: Option<String>,
}

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawVllmFields {
    #[serde(default)]
    block_size_tokens: Option<u32>,
    #[serde(default)]
    max_num_batched_tokens: Option<u32>,
    // ADR 0014 §4 (amended 2026-10-07): `eager` or `lazy`; omitted keeps the
    // capyctl default (`eager` while sleep mode is on).
    #[serde(default)]
    safetensors_load_strategy: Option<SafetensorsLoadStrategy>,
    // ADR 0024: `auto` (the default), `none`, or a parser name.
    #[serde(default)]
    tool_call_parser: Option<String>,
    #[serde(default)]
    reasoning_parser: Option<String>,
}

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSglangFields {
    #[serde(default)]
    max_total_tokens: Option<u32>,
    #[serde(default)]
    chunked_prefill_size: Option<i32>,
    #[serde(default)]
    tokenizer_workers: Option<u32>,
    // ADR 0024: `auto` (the default), `none`, or a parser name.
    #[serde(default)]
    tool_call_parser: Option<String>,
    #[serde(default)]
    reasoning_parser: Option<String>,
}

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTensorfoldFields {
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    thinking: Option<bool>,
}

/// Review decision (discrete GPU design §3): how a memory request derived from
/// the checkpoint's weights is sized when the deployment's phases derive on a
/// device domain (a discrete GPU). Absent everywhere else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DeviceSizing {
    /// The device domain's managed limit: a request above it can never place.
    pub(super) managed_limit: i64,
    /// The card as the host policy declares it: managed limit plus free
    /// reserve (what the standalone host policy publishes as the card's total).
    pub(super) declared_total: i64,
}

/// ADR 0014 amendment A16: the recurrent state a derived SGLang request
/// reserves: that of the most running requests, up to the declared count (or
/// CapyCTL's in-flight bound), whose request still fits the memory domain it
/// derives on, beside the engine's CUDA context and (on unified memory) the
/// first-start graph allowance. `None` with a declared request or
/// `resources:`, without the measured weights and state, or when no running
/// request fits: the launch then sizes the state as before (amendment A14).
fn sglang_state_reserve(
    inputs: &EngineInputs<'_>,
    declared_request: Option<i64>,
    kv_cache: Option<i64>,
    extra_args: &[String],
    declared_running: Option<u32>,
) -> Option<i64> {
    if inputs.engine != Engine::Sglang
        || declared_request.is_some()
        || inputs.declared_ready_total.is_some()
    {
        return None;
    }
    let slot = u64::try_from(inputs.facts.state_slot_bytes?).ok()?;
    let weights = inputs.facts.weights_bytes.filter(|bytes| *bytes > 0)?;
    let kv = kv_cache?;
    let args: Vec<String> = inputs
        .profile_args
        .iter()
        .chain(extra_args)
        .cloned()
        .collect();
    let overhead = if inputs.facts.legacy_overhead {
        0
    } else {
        ENGINE_DEVICE_OVERHEAD_PLACEHOLDER_BYTES
    };
    let graphs = startup_graph_allowance(
        Engine::Sglang,
        inputs.draft_declared
            || crate::engine_policy::draft_model_path(Engine::Sglang, &args).is_some(),
    );
    let fits = |state: u64| {
        let Ok(state) = i64::try_from(state) else {
            return false;
        };
        let (charged, limit) = match inputs.device {
            Some(device) => (
                crate::context_fit::derived_request_bytes(weights, kv, 0, state, true)
                    .and_then(|request| request.checked_add(overhead)),
                Some(device.managed_limit),
            ),
            None => (
                crate::context_fit::derived_request_bytes(
                    weights,
                    kv,
                    host_margin(inputs),
                    state,
                    false,
                )
                .and_then(|request| request.checked_add(overhead)?.checked_add(graphs)),
                inputs.domain_limit,
            ),
        };
        matches!((charged, limit), (Some(charged), Some(limit)) if charged <= limit)
    };
    crate::context_fit::derived_state_reserve(slot, &args, declared_running, fits)
        .and_then(|bytes| i64::try_from(bytes).ok())
}

/// ADR 0014 amendment A18: the margin of a request on memory that is not a
/// discrete GPU's ([`unified_margin`]); a snapshot that records the family
/// margin keeps it.
fn host_margin(inputs: &EngineInputs<'_>) -> i64 {
    if inputs.facts.legacy_family_margin {
        overhead_margin(inputs.engine)
    } else {
        unified_margin(inputs.engine, inputs.facts.weights_bytes)
    }
}

/// Design §3: a device request is `weights x 1.10 + kv`, and vLLM's at least
/// 0.75 of the card (vLLM 0.29 with CUDA graphs does not start a 4B model on a
/// 16 GB card below `--gpu-memory-utilization 0.75`). Unknown weights are not
/// materializable yet: acceptance freezes the revision provisional and the
/// checkpoint digest re-resolves it with the measured weights (ADR 0014 §7),
/// which is how a Hugging Face or HTTP source is sized once downloaded. `None`
/// without a declared KV cache (resolution then asks for one).
fn device_request_from_weights(
    device: DeviceSizing,
    engine: Engine,
    weights: Option<i64>,
    kv_cache: Option<i64>,
) -> Result<Option<i64>, ConfigError> {
    let Some(kv) = kv_cache else {
        return Ok(None);
    };
    let weights = weights.ok_or_else(|| {
        ConfigError::new(
            ConfigErrorCode::NotMaterializable,
            "engine_config.memory.request",
            "cannot size the device request: the checkpoint's weight size is not known yet; \
             it is sized once the checkpoint is measured",
        )
    })?;
    // The standalone template's own arithmetic (`device_request`), so a
    // remote checkpoint sizes exactly as a local one of the same weights.
    let request = (weights / 100)
        .checked_mul(110)
        .and_then(|scaled| scaled.checked_add(kv))
        .ok_or_else(|| invalid("engine_config.memory", "memory arithmetic overflows"))?;
    let floor = match engine {
        Engine::Vllm => device.declared_total / 100 * 75,
        Engine::Sglang | Engine::Tensorfold => 0,
    };
    let request = request.max(floor);
    let charged = request.saturating_add(ENGINE_DEVICE_OVERHEAD_PLACEHOLDER_BYTES);
    if charged > device.managed_limit {
        return Err(invalid(
            "engine_config.memory.request",
            format!(
                "insufficient_device_memory: the deployment needs {charged} bytes of device \
                 memory (a request of {request} bytes, weights x 1.10 plus the KV cache, and \
                 the engine's CUDA context and graphs), above the {} bytes the device domain \
                 manages; use a smaller or quantized checkpoint",
                device.managed_limit
            ),
        ));
    }
    Ok(Some(request))
}

/// Everything besides the block itself that resolution needs.
pub(super) struct EngineInputs<'a> {
    pub(super) engine: Engine,
    pub(super) residency: Residency,
    pub(super) security: &'a Security,
    pub(super) profile_args: &'a [String],
    pub(super) checkpoint_root: Option<&'a Path>,
    /// The Ready phase total of an explicit `resources:` block, if declared.
    pub(super) declared_ready_total: Option<i64>,
    pub(super) facts: CheckpointFacts,
    /// The device domain the phases derive on, when it is a discrete GPU's.
    pub(super) device: Option<DeviceSizing>,
    /// ADR 0014 amendment A16: the managed limit of the one memory domain
    /// the phases derive on, which a derived request's state must leave room
    /// in. `None` when the devices name no single domain.
    pub(super) domain_limit: Option<i64>,
    /// ADR 0008 amendment 2026-10-08: the deployment declares its drafter's
    /// source (`model.draft`), which CapyCTL renders to the engine.
    pub(super) draft_declared: bool,
}

/// ADR 0014 §5 (P2) inputs, all in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryInputs {
    pub request: Option<i64>,
    pub kv_cache: Option<i64>,
    /// The Ready total of an explicit `resources:` block.
    pub declared_ready_total: Option<i64>,
    pub weights: Option<i64>,
    pub margin: i64,
}

/// A resolved memory request and the fields that were derived rather than declared.
pub type ResolvedMemory = (MemoryRequest, Vec<(&'static str, SettingSource)>);

/// ADR 0014 §5: resolve the per-instance memory request and KV cache.
///
/// The request is declared, taken from an explicit `resources:` Ready total, or
/// derived as weights + KV + margin. The KV cache is declared or derived as
/// request − weights − margin and must be positive. At least one of the two is
/// required: an engine's own default takes all free memory (SPEC §7.1).
/// Returns the request and which fields were derived.
pub fn resolve_memory(inputs: MemoryInputs) -> Result<ResolvedMemory, ConfigError> {
    const PATH: &str = "engine_config.memory";
    let overflow = || invalid(PATH, "memory arithmetic overflows");
    if inputs.request.is_none()
        && inputs.kv_cache.is_none()
        && inputs.declared_ready_total.is_none()
    {
        return Err(ConfigError::new(
            ConfigErrorCode::MissingRequired,
            PATH,
            "declare memory.request or memory.kv_cache: an engine's own default takes all \
             free memory, and capyctl must know a deployment's memory before it starts",
        ));
    }
    for (path, value) in [
        ("engine_config.memory.request", inputs.request),
        ("engine_config.memory.kv_cache", inputs.kv_cache),
    ] {
        if value.is_some_and(|bytes| bytes <= 0) {
            return Err(invalid(path, "must be positive"));
        }
    }
    if inputs.weights.is_some_and(|bytes| bytes < 0) || inputs.margin < 0 {
        return Err(invalid(PATH, "weights and margin must not be negative"));
    }
    if let (Some(request), Some(total)) = (inputs.request, inputs.declared_ready_total) {
        if request != total {
            return Err(invalid(
                "engine_config.memory.request",
                "must equal the Ready allocation total when resources are declared",
            ));
        }
    }
    let mut derived = Vec::new();
    let request = match (inputs.request, inputs.declared_ready_total) {
        (Some(request), _) => request,
        (None, Some(total)) => {
            derived.push(("memory.request", SettingSource::Derived));
            total
        }
        (None, None) => {
            let weights = inputs.weights.ok_or_else(|| {
                ConfigError::new(
                    ConfigErrorCode::NotMaterializable,
                    "engine_config.memory.request",
                    "cannot derive the memory request: the checkpoint's weight size is not \
                     known yet; declare memory.request",
                )
            })?;
            let kv = inputs
                .kv_cache
                .expect("request or kv_cache is declared here");
            derived.push(("memory.request", SettingSource::Derived));
            weights
                .checked_add(kv)
                .and_then(|sum| sum.checked_add(inputs.margin))
                .ok_or_else(overflow)?
        }
    };
    if request <= 0 {
        return Err(invalid("engine_config.memory.request", "must be positive"));
    }
    let kv_cache = match inputs.kv_cache {
        Some(kv) => kv,
        None => {
            let weights = inputs.weights.ok_or_else(|| {
                ConfigError::new(
                    ConfigErrorCode::NotMaterializable,
                    "engine_config.memory.kv_cache",
                    "cannot derive the KV cache: the checkpoint's weight size is not known \
                     yet; declare memory.kv_cache",
                )
            })?;
            let kv = request
                .checked_sub(weights)
                .and_then(|rest| rest.checked_sub(inputs.margin))
                .ok_or_else(overflow)?;
            if kv <= 0 {
                return Err(invalid(
                    "engine_config.memory.kv_cache",
                    "the derived KV cache (request minus weights minus margin) is not positive",
                ));
            }
            derived.push(("memory.kv_cache", SettingSource::Derived));
            kv
        }
    };
    // Spec §3: admission reserves the request before the engine starts. A KV
    // cache larger than that reservation would hand the engine memory nothing
    // accounted for; the overrun would appear as an out-of-memory kill well
    // after the deployment was accepted.
    if kv_cache > request {
        return Err(invalid(
            "engine_config.memory.kv_cache",
            "the KV cache exceeds the memory request admission accounts for",
        ));
    }
    if let Some(weights) = inputs.weights {
        if weights.checked_add(kv_cache).ok_or_else(overflow)? > request {
            return Err(invalid(
                PATH,
                "the checkpoint's weights plus the KV cache exceed the memory request",
            ));
        }
    }
    Ok((
        MemoryRequest {
            request_bytes: request,
            kv_cache_bytes: kv_cache,
            margin_bytes: inputs.margin,
            weights_bytes: inputs.weights,
            startup_bytes: None,
            device_total_bytes: None,
            overhead_bytes: None,
            startup_graphs_bytes: None,
            state_slot_bytes: None,
            state_bytes: None,
            member: None,
            disk_tables: None,
        },
        derived,
    ))
}

/// Owner decision 2026-09-23 (startup memory budget): the startup peak and,
/// when capyctl chose it, its provenance.
///
/// A declared `memory.startup` is the Initialize peak and cannot be below the
/// steady request. A deployment that declares its `resources:` phases states
/// its cold phase there instead, so it declares no startup and none is derived.
/// Otherwise the placeholder default applies until a first run on a host
/// measures the peak (the store keeps that measurement per revision, host and
/// installation and prefers it over this default).
pub fn resolve_startup(
    declared: Option<i64>,
    memory: &MemoryRequest,
    resources_declared: bool,
    facts: CheckpointFacts,
    margin: i64,
    graphs: Option<i64>,
) -> Result<(Option<i64>, Option<SettingSource>), ConfigError> {
    const PATH: &str = "engine_config.memory.startup";
    if let Some(peak) = declared {
        if resources_declared {
            return Err(invalid(
                PATH,
                "a deployment that declares its resources states its startup peak as the cold \
                 phase; remove memory.startup or the resources block",
            ));
        }
        if peak <= 0 {
            return Err(invalid(PATH, "must be positive"));
        }
        if peak < memory.request_bytes {
            return Err(invalid(
                PATH,
                "the startup peak cannot be below the steady memory request",
            ));
        }
        return Ok((Some(peak), None));
    }
    if resources_declared || facts.legacy_startup {
        return Ok((None, None));
    }
    let peak = default_startup_bytes(memory.request_bytes, memory.weights_bytes, margin, graphs)
        .ok_or_else(|| invalid(PATH, "memory arithmetic overflows"))?;
    Ok((Some(peak), Some(SettingSource::Derived)))
}

/// ADR 0014 §5: derived phases. Ready, parking and wake equal the request and
/// cold is the startup peak (never below the request);
/// parked is the residual floor placeholder for a parking deployment and zero
/// for one that restarts. Derivation needs one memory domain for the selected
/// devices; anything else declares `resources:` explicitly. On a discrete host
/// (the selected device's domain is a `device` domain) every phase charges the
/// device domain and the system domain (discrete GPU design §3).
pub(super) fn derive_resources(
    request: i64,
    startup: Option<i64>,
    residency: Residency,
    devices: &[DeviceClaim],
    host: &HostPolicy,
    weights_bytes: Option<i64>,
    overhead: i64,
) -> Result<RecipeFootprints, ConfigError> {
    let mut domains = BTreeSet::new();
    for claim in devices {
        let policy = host
            .devices
            .get(&claim.id)
            .ok_or_else(|| invalid("devices", "unknown device"))?;
        domains.insert(policy.domain.clone());
    }
    let domain =
        match domains.len() {
            1 => domains.into_iter().next().expect("one domain"),
            0 => {
                return Err(invalid(
                    "devices",
                    "deriving resources from engine_config.memory needs a selected device",
                ))
            }
            _ => return Err(invalid(
                "resources",
                "the selected devices span several memory domains; declare resources explicitly",
            )),
        };
    // Owner decision 2026-09-23: admission reserves the startup peak from arm
    // until Ready (the cold phase, ADR 0007); Ready drops to the request.
    let cold = startup.map_or(request, |peak| peak.max(request));
    if host.domains.get(&domain).map(|d| d.memory) == Some(DomainMemory::Device) {
        return derive_discrete(DiscreteInputs {
            request,
            cold,
            residency,
            devices,
            host,
            device_domain: domain,
            weights_bytes,
            overhead,
        });
    }
    // Re-review (parity rule): the engine's CUDA context and graphs sit
    // outside its request in a unified pool as on a card, so the pool is
    // charged them too, by the rule `derive_discrete` applies.
    let on_pool = |bytes: i64| {
        bytes
            .checked_add(overhead)
            .ok_or_else(|| invalid("resources", "memory arithmetic overflows"))
    };
    let (request_charge, cold) = (on_pool(request)?, on_pool(cold)?);
    let active = |bytes: i64| PhaseFootprint {
        allocations: vec![Allocation {
            domain: domain.clone(),
            bytes,
            host_kv_bytes: 0,
        }],
        devices: devices.to_vec(),
    };
    let parked_bytes = if residency.parks() {
        PARKED_RESIDUAL_PLACEHOLDER_BYTES.min(request)
    } else {
        0
    };
    Ok(RecipeFootprints {
        cold: active(cold),
        ready: active(request_charge),
        parking: active(request_charge),
        parked: PhaseFootprint {
            allocations: vec![Allocation {
                domain: domain.clone(),
                bytes: parked_bytes,
                host_kv_bytes: 0,
            }],
            devices: Vec::new(),
        },
        wake: active(request_charge),
    })
}

struct DiscreteInputs<'a> {
    request: i64,
    cold: i64,
    residency: Residency,
    devices: &'a [DeviceClaim],
    host: &'a HostPolicy,
    device_domain: String,
    weights_bytes: Option<i64>,
    overhead: i64,
}

/// Discrete GPU design §3 (ADR 0019): VRAM in the device domain, the engine's host
/// overhead (and the `host_backed` weights copy) in host RAM, the one `distinct`
/// system domain. Every phase carries both allocations, `[device, system]`.
fn derive_discrete(inputs: DiscreteInputs<'_>) -> Result<RecipeFootprints, ConfigError> {
    let DiscreteInputs {
        request,
        cold,
        residency,
        devices,
        host,
        device_domain,
        weights_bytes,
        overhead: context_overhead,
    } = inputs;
    let systems: Vec<&String> = host
        .domains
        .iter()
        .filter(|(_, d)| d.memory == DomainMemory::Distinct)
        .map(|(name, _)| name)
        .collect();
    let [system] = systems.as_slice() else {
        return Err(invalid(
            "resource_policy.domains",
            "missing_system_allocation: a discrete host declares one distinct system domain",
        ));
    };
    // Discrete GPU design §3: the host_backed copy is the checkpoint's weight
    // bytes. Unknown weights leave nothing to charge, and an uncharged copy is an
    // overcommit of host RAM, so the footprint is not materializable until the
    // checkpoint digest measures them (ADR 0014 §5, §7): acceptance freezes it
    // provisional and re-resolves it with the measured weights, exactly as a
    // memory request derived from the weights.
    let copy = match residency {
        Residency::HostBacked => weights_bytes.map(host_backed_copy_bytes).ok_or_else(|| {
            ConfigError::new(
                ConfigErrorCode::NotMaterializable,
                "engine_config.memory",
                "cannot charge the host_backed weights copy: the checkpoint's weight size \
                 is not known yet",
            )
        })?,
        _ => 0,
    };
    // SGLang's --enable-weights-cpu-backup holds the copy for the engine's
    // life. So does vLLM 0.29 in practice: level 1 frees its backup tensors on
    // wake, but PyTorch's pinned allocator keeps the memory cached (measured
    // live on a 16 GB discrete GPU: the process's shared memory stayed at the
    // copy's size after the wake), so both are charged it in every phase.
    let always = copy;
    let overhead = ENGINE_HOST_OVERHEAD_PLACEHOLDER_BYTES;
    // The engine holds its CUDA context and graphs on the card beside the
    // request (measured live: 13.2 GiB held against a 12.0 GiB request), so
    // the device domain is charged both, and planner, admission and the
    // launch check judge the same figure.
    let on_card = |bytes: i64| {
        bytes
            .checked_add(context_overhead)
            .ok_or_else(|| invalid("resources", "memory arithmetic overflows"))
    };
    let active = on_card(request)?;
    let cold = on_card(cold)?;
    if let Some(limit) = host
        .domains
        .get(&device_domain)
        .map(|domain| domain.managed_limit)
        .filter(|limit| active > *limit)
    {
        return Err(invalid(
            "engine_config.memory.request",
            format!(
                "insufficient_device_memory: the deployment needs {active} bytes of device \
                 memory (a request of {request} bytes and the engine's CUDA context and \
                 graphs), above the {limit} bytes the device domain manages"
            ),
        ));
    }
    let two = |device: i64, system_bytes: i64, devices: Vec<DeviceClaim>| PhaseFootprint {
        allocations: vec![
            Allocation {
                domain: device_domain.clone(),
                bytes: device,
                host_kv_bytes: 0,
            },
            Allocation {
                domain: (*system).clone(),
                bytes: system_bytes,
                host_kv_bytes: 0,
            },
        ],
        devices,
    };
    let add = |a: i64, b: i64| {
        a.checked_add(b)
            .ok_or_else(|| invalid("resources", "memory arithmetic overflows"))
    };
    let steady = add(overhead, always)?;
    let with_copy = add(overhead, copy)?;
    let residue = PARKED_DEVICE_RESIDUE_PLACEHOLDER_BYTES.min(request);
    // The parked phase is what admission counts against each domain's
    // parked_limit, so the copy is charged there as parked residue. Parking and
    // wake are the transitions into and out of it: the copy exists while the
    // weights move, so both carry it too (a transition is never below the
    // phases it joins, `capyctl_domain::resources::validate_recipe`).
    let parked = match residency {
        Residency::RestartOnly => two(0, 0, Vec::new()),
        Residency::Deep => two(residue, overhead, Vec::new()),
        Residency::HostBacked => two(residue, with_copy, Vec::new()),
    };
    Ok(RecipeFootprints {
        cold: two(cold, steady, devices.to_vec()),
        ready: two(active, steady, devices.to_vec()),
        parking: two(active, with_copy, devices.to_vec()),
        parked,
        wake: two(active, with_copy, devices.to_vec()),
    })
}

fn token(path: &str, value: &Option<String>) -> Result<(), ConfigError> {
    if let Some(value) = value {
        if value.is_empty()
            || value.len() > 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            return Err(invalid(path, "must be a short engine value token"));
        }
    }
    Ok(())
}

/// ADR 0024: one declared parser setting. `auto` (or nothing) resolves to
/// `None`, chosen at launch; `none` and a name are kept, and refused beside
/// the same option in the host-fixed or extra args.
fn parser_choice(
    block: &str,
    field: &str,
    value: Option<String>,
    profile_args: &[String],
    extra_args: &[String],
) -> Result<Option<String>, ConfigError> {
    let path = format!("engine_config.{block}.{field}");
    let Some(value) = value.filter(|value| value != crate::parsers::AUTO) else {
        return Ok(None);
    };
    if !crate::parsers::valid_value(&value) {
        return Err(invalid(
            path,
            "must be auto, none, or the engine's parser name",
        ));
    }
    let option = if field == "tool_call_parser" {
        crate::parsers::TOOL_CALL_OPTION
    } else {
        crate::parsers::REASONING_OPTION
    };
    for (args, owner) in [
        (profile_args, "the installation's host-fixed args"),
        (extra_args, "extra_args"),
    ] {
        if crate::parsers::args_set(args, option) {
            return Err(invalid(
                path,
                format!("{owner} already set `{option}`; remove one of them"),
            ));
        }
    }
    Ok(Some(value))
}

fn positive(path: &str, value: Option<u32>) -> Result<(), ConfigError> {
    if value == Some(0) {
        return Err(invalid(path, "must be positive"));
    }
    Ok(())
}

/// ADR 0014 §2–§6: resolve a deployment's `engine_config` against the selected
/// installation and residency.
pub(super) fn normalize_engine_config(
    raw: RawEngineConfig,
    inputs: EngineInputs<'_>,
) -> Result<LaunchSettings, ConfigError> {
    let engine = inputs.engine;
    let family_mismatch = |block: &str| {
        invalid(
            format!("engine_config.{block}"),
            "this block applies to another engine family than the selected runtime profile",
        )
    };
    let foreign: &[(&str, bool)] = &[
        ("vllm", raw.vllm.is_some()),
        ("sglang", raw.sglang.is_some()),
        ("tensorfold", raw.tensorfold.is_some()),
    ];
    for (block, present) in foreign {
        if *present && *block != engine.name() {
            return Err(family_mismatch(block));
        }
    }
    // ADR 0023 §4: TensorFold has no flag for these common fields.
    if engine == Engine::Tensorfold {
        for (field, set) in [
            ("dtype", raw.dtype.is_some()),
            ("quantization", raw.quantization.is_some()),
            ("cuda_graphs", raw.cuda_graphs.is_some()),
            // `false` is what TensorFold does, and what a snapshot restates.
            ("language_model_only", raw.language_model_only == Some(true)),
            ("trust_remote_code", raw.trust_remote_code == Some(true)),
        ] {
            if set {
                return Err(invalid(
                    format!("engine_config.{field}"),
                    "TensorFold has no option for this field; remove it",
                ));
            }
        }
        if raw.context_length.is_none() {
            return Err(ConfigError::new(
                ConfigErrorCode::MissingRequired,
                "engine_config.context_length",
                "a TensorFold deployment states context_length: it fixes the window \
                 TensorFold serves",
            ));
        }
    }
    if let Some(dtype) = &raw.dtype {
        if !DTYPES.contains(&dtype.as_str()) {
            return Err(invalid(
                "engine_config.dtype",
                "must be one of auto, bfloat16, float16, float32",
            ));
        }
    }
    token("engine_config.quantization", &raw.quantization)?;
    token("engine_config.kv_cache_dtype", &raw.kv_cache_dtype)?;
    positive("engine_config.context_length", raw.context_length)?;
    positive(
        "engine_config.max_concurrent_requests",
        raw.max_concurrent_requests,
    )?;
    let trust_remote_code = raw.trust_remote_code.unwrap_or(false);
    // Spec §3: remote code executes Python shipped with the checkpoint. The
    // host installation keeps its switch (ADR 0014 §8).
    if trust_remote_code && !inputs.security.trust_remote_code {
        return Err(invalid(
            "engine_config.trust_remote_code",
            "trust_remote_code executes code shipped with the checkpoint; it requires \
             security.trust_remote_code: true on the installation",
        ));
    }

    // Host-fixed profile arguments and deployment typed fields must not both
    // set one engine option (ADR 0014 §2).
    let host_fixed = option_names(inputs.profile_args)
        .map_err(|error| invalid("runtime_profiles.args", error.to_string()))?;
    let vllm = raw.vllm.clone().unwrap_or_default();
    let sglang = raw.sglang.clone().unwrap_or_default();
    let tensorfold = raw.tensorfold.clone().unwrap_or_default();
    let declared: [(&str, bool); 16] = [
        ("dtype", raw.dtype.is_some()),
        ("quantization", raw.quantization.is_some()),
        ("kv_cache_dtype", raw.kv_cache_dtype.is_some()),
        ("context_length", raw.context_length.is_some()),
        (
            "max_concurrent_requests",
            raw.max_concurrent_requests.is_some(),
        ),
        ("cuda_graphs", raw.cuda_graphs.is_some()),
        ("language_model_only", raw.language_model_only.is_some()),
        ("trust_remote_code", raw.trust_remote_code.is_some()),
        ("vllm.block_size_tokens", vllm.block_size_tokens.is_some()),
        (
            "vllm.max_num_batched_tokens",
            vllm.max_num_batched_tokens.is_some(),
        ),
        (
            "vllm.safetensors_load_strategy",
            vllm.safetensors_load_strategy.is_some(),
        ),
        ("sglang.max_total_tokens", sglang.max_total_tokens.is_some()),
        (
            "sglang.chunked_prefill_size",
            sglang.chunked_prefill_size.is_some(),
        ),
        (
            "sglang.tokenizer_workers",
            sglang.tokenizer_workers.is_some(),
        ),
        ("tensorfold.max_tokens", tensorfold.max_tokens.is_some()),
        ("tensorfold.thinking", tensorfold.thinking.is_some()),
    ];
    for (field, is_set) in declared {
        if let Some(option) = typed_field_option(engine, field) {
            if is_set && host_fixed.contains(option) {
                return Err(invalid(
                    format!("engine_config.{field}"),
                    format!(
                        "the installation's host-fixed args already set `{option}`; \
                         remove one of them"
                    ),
                ));
            }
        }
    }

    // SPEC §6.2, §9.1 / ADR 0012: sleep mode, memory saver and CPU weight backup
    // are derived from residency and the host's deep-park switch, never declared.
    let parks = inputs.residency.parks();
    let sleep_mode = parks && inputs.security.deep_park.is_enabled();

    // ADR 0014 §6 (owner decision Q10).
    let extra_args = raw.extra_args.clone().unwrap_or_default();
    if !extra_args.is_empty() {
        if raw.accept_extra_args != Some(true) {
            return Err(invalid(
                "engine_config.extra_args",
                "extra engine arguments need engine_config.accept_extra_args: true",
            ));
        }
        if inputs.security.extra_args == ExtraArgsPolicy::Denied {
            return Err(invalid(
                "engine_config.extra_args",
                "this installation denies extra engine arguments (security.extra_args: denied)",
            ));
        }
        let approved_options = inputs
            .security
            .approved_options
            .iter()
            .map(|name| crate::engine_policy::normalize_option_name(name))
            .collect();
        let approved_paths: Vec<PathBuf> = inputs
            .security
            .approved_paths
            .iter()
            .map(PathBuf::from)
            .collect();
        validate_extra_args(
            &extra_args,
            &ExtraArgsContext {
                engine,
                sleep_mode: engine == Engine::Vllm && sleep_mode,
                approved_options: &approved_options,
                approved_paths: &approved_paths,
                checkpoint_root: inputs.checkpoint_root,
                host_fixed: &host_fixed,
            },
        )
        .map_err(|error| invalid("engine_config.extra_args", error.to_string()))?;
    }
    // ADR 0023 §4 (amended 2026-10-03): a declared count renders TensorFold's
    // `--parallel`; the same option passed beside it says it twice.
    if engine == Engine::Tensorfold && raw.max_concurrent_requests.is_some() {
        for (args, whose) in [
            (inputs.profile_args, "the installation's host-fixed args"),
            (extra_args.as_slice(), "engine_config.extra_args"),
        ] {
            let passed = crate::engine_policy::tensorfold_parallel(args)
                .map_err(|error| invalid("engine_config.extra_args", error.to_string()))?;
            if passed.is_some() {
                return Err(invalid(
                    "engine_config.max_concurrent_requests",
                    format!(
                        "{whose} already set `{}`, which is TensorFold's option for this \
                         field; remove one of them",
                        crate::engine_policy::TENSORFOLD_PARALLEL
                    ),
                ));
            }
        }
    }
    // ADR 0024: a parser the deployment names (or turns off) and the same
    // option in the host-fixed or extra args contradict each other.
    let parser_fields = match engine {
        Engine::Vllm => Some((
            "vllm",
            vllm.tool_call_parser.clone(),
            vllm.reasoning_parser.clone(),
        )),
        Engine::Sglang => Some((
            "sglang",
            sglang.tool_call_parser.clone(),
            sglang.reasoning_parser.clone(),
        )),
        Engine::Tensorfold => None,
    };
    let (tool_call_parser, reasoning_parser) = match parser_fields {
        Some((block, tool, reasoning)) => (
            parser_choice(
                block,
                "tool_call_parser",
                tool,
                inputs.profile_args,
                &extra_args,
            )?,
            parser_choice(
                block,
                "reasoning_parser",
                reasoning,
                inputs.profile_args,
                &extra_args,
            )?,
        ),
        None => (None, None),
    };
    // ADR 0023 §5: drafts off and a named drafter contradict each other,
    // whether the host-fixed or the extra arguments say either.
    if engine == Engine::Tensorfold {
        let all: Vec<String> = inputs
            .profile_args
            .iter()
            .chain(&extra_args)
            .cloned()
            .collect();
        let drafts = crate::engine_policy::tensorfold_drafts(&all)
            .map_err(|error| invalid("engine_config.extra_args", error.to_string()))?;
        if drafts.names_drafter && drafts.drafts_off {
            return Err(invalid(
                "engine_config.extra_args",
                crate::engine_policy::TENSORFOLD_DRAFTS_CONFLICT,
            ));
        }
    }
    // ADR 0008 amendment 2026-10-08: a declared drafter's path is CapyCTL's
    // to render, so the arguments may name no other draft model, and they
    // turn speculation on where the engine needs it (host-fixed or extra).
    if inputs.draft_declared {
        let all: Vec<String> = inputs
            .profile_args
            .iter()
            .chain(&extra_args)
            .cloned()
            .collect();
        crate::engine_policy::declared_draft_admitted(engine, &all)
            .map_err(|reason| invalid("model.draft", reason))?;
    }

    let raw_memory = raw.memory.clone().unwrap_or_default();
    let declared_request = raw_memory.request.as_deref().map(parse_bytes).transpose()?;
    let kv_cache = raw_memory
        .kv_cache
        .as_deref()
        .map(parse_bytes)
        .transpose()?;
    let mut declared_startup = raw_memory.startup.as_deref().map(parse_bytes).transpose()?;
    // ADR 0023 §4: TensorFold is told no KV size; `--context` fixes it inside
    // the declared reservation, which is all an undeclared KV cache is bounded by.
    let tensorfold_kv = engine == Engine::Tensorfold && kv_cache.is_none();
    let kv_cache = if tensorfold_kv {
        inputs.declared_ready_total
    } else {
        kv_cache
    };
    // Review decision (discrete GPU design §3): a request derived from the
    // weights on a device domain is sized for the card, not with the unified
    // placeholder margin, which would not fit a small card.
    let mut provenance_startup_derived = false;
    // ADR 0014 amendment A16: a derived SGLang request holds the recurrent
    // state of its running requests, when the checkpoint's state is known.
    let state_reserve = sglang_state_reserve(
        &inputs,
        declared_request,
        kv_cache,
        &extra_args,
        raw.max_concurrent_requests,
    );
    let device_request = match (inputs.device, declared_request, inputs.declared_ready_total) {
        (Some(device), None, None) => device_request_from_weights(
            device,
            engine,
            inputs
                .facts
                .weights_bytes
                .map(|weights| weights.saturating_add(state_reserve.unwrap_or(0))),
            kv_cache,
        )?,
        _ => None,
    };
    // ADR 0019 §3 (found live on a 16 GB laptop GPU, 2026-10-03): a request
    // declared for a discrete GPU is sized as the derived one is: the margin
    // is the weights x 0.10 of `device_request_from_weights`, so a KV cache
    // left out is the request less the weights x 1.10, and an undeclared
    // startup peak is the request. A revision frozen before records the
    // family margin and re-resolves as it was.
    let declared_device_margin = match (inputs.device, declared_request, inputs.facts.weights_bytes)
    {
        (Some(_), Some(_), Some(weights)) if !inputs.facts.legacy_family_margin => {
            Some(weights / 100 * 10)
        }
        _ => None,
    };
    // ADR 0014 amendment A18 (owner decision 2026-10-07): on unified memory
    // the margin holds the engine's CPU-side memory as well as its GPU memory
    // beyond the weights and the KV cache, and grows with the weights.
    let margin = match (declared_device_margin, inputs.device) {
        (Some(margin), _) => margin,
        (None, Some(_)) => overhead_margin(engine),
        (None, None) => host_margin(&inputs),
    };
    let (mut memory, memory_provenance) = resolve_memory(MemoryInputs {
        request: device_request.or(declared_request),
        kv_cache,
        declared_ready_total: inputs.declared_ready_total,
        weights: inputs.facts.weights_bytes,
        margin,
    })?;
    if engine == Engine::Sglang {
        memory.state_slot_bytes = inputs.facts.state_slot_bytes;
    }
    if let Some(state) = state_reserve {
        // A discrete request already holds it in its weights share.
        if device_request.is_none() {
            memory.request_bytes = memory
                .request_bytes
                .checked_add(state)
                .ok_or_else(|| invalid("engine_config.memory", "memory arithmetic overflows"))?;
        }
        memory.state_bytes = Some(state);
    }
    if declared_device_margin.is_some() && declared_startup.is_none() {
        // Design §3: the engine's use of the card is bounded by the fraction
        // capyctl renders from this request.
        declared_startup = Some(memory.request_bytes);
        provenance_startup_derived = true;
    }
    let mut provenance: BTreeMap<String, SettingSource> = memory_provenance
        .into_iter()
        .map(|(field, source)| (field.to_owned(), source))
        .collect();
    if tensorfold_kv {
        provenance.insert("memory.kv_cache".into(), SettingSource::Derived);
    }
    if provenance_startup_derived {
        provenance.insert("memory.startup".into(), SettingSource::Derived);
    }
    if let Some(request) = device_request {
        provenance.insert("memory.request".into(), SettingSource::Derived);
        // Design §3: the engine's use of the card is bounded by the fraction
        // capyctl renders from this request, so the device peak is the request.
        if declared_startup.is_none() {
            declared_startup = Some(request);
            provenance.insert("memory.startup".into(), SettingSource::Derived);
        }
    }
    // ADR 0014 amendment A8: a derived placeholder covers the CUDA graphs the
    // first start captures, the draft model's too; a revision frozen before
    // the allowance re-resolves without it.
    let graphs = if inputs.facts.legacy_startup_graphs {
        None
    } else {
        let all: Vec<String> = inputs
            .profile_args
            .iter()
            .chain(&extra_args)
            .cloned()
            .collect();
        Some(startup_graph_allowance(
            engine,
            inputs.draft_declared || crate::engine_policy::draft_model_path(engine, &all).is_some(),
        ))
    };
    let (startup, startup_source) = resolve_startup(
        declared_startup,
        &memory,
        inputs.declared_ready_total.is_some(),
        inputs.facts,
        overhead_margin(engine),
        graphs,
    )?;
    memory.startup_bytes = startup;
    if let Some(source) = startup_source {
        provenance.insert("memory.startup".into(), source);
        // Only the placeholder default is derived here; a device request's
        // startup was set above as declared.
        memory.startup_graphs_bytes = graphs;
    }

    let mut common = CommonEngineSettings {
        dtype: raw.dtype,
        quantization: raw.quantization,
        kv_cache_dtype: raw.kv_cache_dtype,
        context_length: raw.context_length,
        max_concurrent_requests: raw.max_concurrent_requests,
        cuda_graphs: raw.cuda_graphs,
        language_model_only: raw.language_model_only.unwrap_or(false),
        trust_remote_code,
    };
    let settings = match engine {
        Engine::Vllm => {
            positive(
                "engine_config.vllm.block_size_tokens",
                vllm.block_size_tokens,
            )?;
            positive(
                "engine_config.vllm.max_num_batched_tokens",
                vllm.max_num_batched_tokens,
            )?;
            provenance.insert("enable_sleep_mode".into(), SettingSource::Derived);
            LaunchSettings::Vllm(VllmLaunchSettings {
                common,
                memory,
                block_size_tokens: vllm.block_size_tokens,
                max_num_batched_tokens: vllm.max_num_batched_tokens,
                safetensors_load_strategy: vllm.safetensors_load_strategy,
                tool_call_parser,
                reasoning_parser,
                enable_sleep_mode: sleep_mode,
                extra_args,
                provenance,
            })
        }
        Engine::Sglang => {
            positive(
                "engine_config.sglang.max_total_tokens",
                sglang.max_total_tokens,
            )?;
            positive(
                "engine_config.sglang.tokenizer_workers",
                sglang.tokenizer_workers,
            )?;
            if sglang
                .chunked_prefill_size
                .is_some_and(|size| size == 0 || size < -1)
            {
                return Err(invalid(
                    "engine_config.sglang.chunked_prefill_size",
                    "must be positive, or -1 to disable chunked prefill",
                ));
            }
            // SGLang takes its park strategy at launch: the memory saver and the
            // weights CPU backup cannot be added to a running engine, so the
            // declared tier reaches the launch settings (ADR 0010).
            let memory_saver = parks;
            let cpu_weight_backup = inputs.residency == Residency::HostBacked;
            // ADR 0014 amendment A17: with speculative decoding the park keeps
            // the weights resident and releases the KV cache alone. SGLang's
            // weight release takes the draft model's weights too, and its disk
            // reload would load the target's checkpoint into the draft.
            let speculative = crate::engine_policy::sglang_speculative(
                &inputs
                    .profile_args
                    .iter()
                    .chain(&extra_args)
                    .cloned()
                    .collect::<Vec<_>>(),
            );
            let weight_restore = if cpu_weight_backup {
                "cpu_backup"
            } else if memory_saver && speculative {
                "resident"
            } else {
                "disk_reload"
            };
            for field in ["memory_saver", "cpu_weight_backup", "weight_restore"] {
                provenance.insert(field.into(), SettingSource::Derived);
            }
            // ADR 0014 amendment A13: CUDA graphs are SGLang's default (on)
            // while the memory saver is on too; what a parked engine keeps of
            // them is measured and charged per revision. A revision frozen
            // under the old default (graphs off beside the saver) keeps it.
            if common.cuda_graphs.is_none() && memory_saver && inputs.facts.legacy_sglang_graphs_off
            {
                common.cuda_graphs = Some(false);
                provenance.insert("cuda_graphs".into(), SettingSource::CapyctlDefault);
            }
            let tokenizer_workers = match sglang.tokenizer_workers {
                Some(workers) => workers,
                None => {
                    provenance.insert(
                        "sglang.tokenizer_workers".into(),
                        SettingSource::CapyctlDefault,
                    );
                    1
                }
            };
            LaunchSettings::Sglang(SglangLaunchSettings {
                common,
                memory,
                max_total_tokens: sglang.max_total_tokens,
                max_mamba_cache_size: None,
                static_allowance_bytes: None,
                chunked_prefill_size: sglang.chunked_prefill_size,
                tokenizer_workers,
                tool_call_parser,
                reasoning_parser,
                memory_saver,
                cpu_weight_backup,
                weight_restore: weight_restore.into(),
                extra_args,
                provenance,
            })
        }
        Engine::Tensorfold => {
            positive("engine_config.tensorfold.max_tokens", tensorfold.max_tokens)?;
            LaunchSettings::Tensorfold(TensorfoldLaunchSettings {
                common,
                memory,
                max_tokens: tensorfold.max_tokens,
                thinking: tensorfold.thinking,
                extra_args,
                provenance,
            })
        }
    };
    Ok(settings)
}

/// The declared, unresolved identity of an `engine_config` block for command
/// fingerprints: typed and parsed, with nothing derived or defaulted.
pub(super) fn declared_engine_config(raw: &RawEngineConfig) -> Result<Value, ConfigError> {
    let memory = raw.memory.clone().unwrap_or_default();
    let mut declared = serde_json::json!({
        "dtype": raw.dtype, "quantization": raw.quantization,
        "kv_cache_dtype": raw.kv_cache_dtype, "context_length": raw.context_length,
        "max_concurrent_requests": raw.max_concurrent_requests,
        "cuda_graphs": raw.cuda_graphs, "language_model_only": raw.language_model_only,
        "trust_remote_code": raw.trust_remote_code,
        "memory": declared_memory(&memory)?,
        "vllm": raw.vllm.as_ref().map(|v| {
            let mut block = with_parsers(serde_json::json!({
                "block_size_tokens": v.block_size_tokens,
                "max_num_batched_tokens": v.max_num_batched_tokens,
            }), &v.tool_call_parser, &v.reasoning_parser);
            // ADR 0014 §4 (amended 2026-10-07): present only when declared,
            // so every deployment written before the setting keeps its identity.
            if let Some(strategy) = v.safetensors_load_strategy {
                block["safetensors_load_strategy"] = serde_json::json!(strategy);
            }
            block
        }),
        "sglang": raw.sglang.as_ref().map(|s| with_parsers(serde_json::json!({
            "max_total_tokens": s.max_total_tokens,
            "chunked_prefill_size": s.chunked_prefill_size,
            "tokenizer_workers": s.tokenizer_workers,
        }), &s.tool_call_parser, &s.reasoning_parser)),
        "tensorfold": raw.tensorfold.as_ref().map(|t| serde_json::json!({
            "max_tokens": t.max_tokens, "thinking": t.thinking,
        })),
        "accept_extra_args": raw.accept_extra_args,
        "extra_args": raw.extra_args,
    });
    // ADR 0023 §4: the key appears only when declared, so every vLLM and
    // SGLang command fingerprint keeps its identity.
    if raw.tensorfold.is_none() {
        if let Some(object) = declared.as_object_mut() {
            object.remove("tensorfold");
        }
    }
    // ADR 0028 §2.1: likewise the environment, only when declared.
    if !raw.env.is_empty() {
        if let Some(object) = declared.as_object_mut() {
            object.insert("env".into(), serde_json::json!(raw.env));
        }
    }
    Ok(declared)
}

/// ADR 0024: the parser settings join a block's identity only when declared,
/// so every earlier command fingerprint keeps its identity.
fn with_parsers(mut block: Value, tool: &Option<String>, reasoning: &Option<String>) -> Value {
    for (key, value) in [("tool_call_parser", tool), ("reasoning_parser", reasoning)] {
        if let Some(value) = value {
            block[key] = serde_json::json!(value);
        }
    }
    block
}

/// The declared memory block's identity. `startup` appears only when declared,
/// so every deployment written before the startup budget keeps its identity.
fn declared_memory(memory: &RawMemory) -> Result<Value, ConfigError> {
    let mut block = serde_json::json!({
        "request": memory.request.as_deref().map(parse_bytes).transpose()?,
        "kv_cache": memory.kv_cache.as_deref().map(parse_bytes).transpose()?,
    });
    if let Some(startup) = memory.startup.as_deref() {
        block["startup"] = serde_json::json!(parse_bytes(startup)?);
    }
    Ok(block)
}

/// Reject host documents that still carry moved profile fields, with a pointer
/// to where the setting lives now (ADR 0014 §1, SPEC §15.3 strictness). The
/// strict YAML walker gives the same message; this covers JSON inputs that
/// reach resolution directly.
pub(super) fn refuse_moved_profile_fields(host: &Value) -> Result<(), ConfigError> {
    if let Some(profiles) = host.get("runtime_profiles").and_then(Value::as_object) {
        for (name, profile) in profiles {
            if profile.get("launch_settings").is_some() {
                return Err(ConfigError::new(
                    ConfigErrorCode::UnknownField,
                    format!("host.runtime_profiles.{name}.launch_settings"),
                    crate::schema::LAUNCH_SETTINGS_MOVED,
                ));
            }
        }
    }
    Ok(())
}

use serde_json::Value;
