//! ADR 0014: resolution of a deployment's `engine_config` into launch settings.
//!
//! Owner decisions E1 and P2: every engine serves any model; the deployment
//! chooses its parameters. mllm validates type, range and ownership here, not
//! whether the engine supports a value on this checkpoint (ADR 0011).

use super::*;
use crate::engine_policy::{
    option_names, typed_field_option, validate_extra_args, ExtraArgsContext, ExtraArgsPolicy,
};
use mllm_domain::launch::{
    CommonEngineSettings, LaunchSettings, MemoryRequest, SettingSource, SglangLaunchSettings,
    VllmLaunchSettings,
};

/// ADR 0014 §5: conservative placeholder overhead margins, per engine family,
/// until M16 measures peak minus weights minus KV for each model and engine.
pub const VLLM_OVERHEAD_MARGIN_BYTES: i64 = 8 << 30;
pub const SGLANG_OVERHEAD_MARGIN_BYTES: i64 = 8 << 30;

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
/// of the factor 1.6.
pub const STARTUP_WEIGHTS_FACTOR: (i64, i64) = (8, 5);

/// Owner decision 2026-09-23: the placeholder startup peak for a request,
/// the checkpoint's weights (when known) and the family margin.
pub fn default_startup_bytes(request: i64, weights: Option<i64>, margin: i64) -> Option<i64> {
    let Some(weights) = weights else {
        return Some(request);
    };
    let (numerator, denominator) = STARTUP_WEIGHTS_FACTOR;
    weights
        .checked_mul(numerator)
        .map(|scaled| scaled / denominator)
        .and_then(|scaled| scaled.checked_add(margin))
        .map(|peak| peak.max(request))
}

/// The per-family overhead margin a derived memory request adds.
pub fn overhead_margin(engine: Engine) -> i64 {
    match engine {
        Engine::Vllm => VLLM_OVERHEAD_MARGIN_BYTES,
        Engine::Sglang => SGLANG_OVERHEAD_MARGIN_BYTES,
    }
}

/// ADR 0014 §2: the dtypes mllm accepts by name; anything else is refused.
const DTYPES: &[&str] = &["auto", "bfloat16", "float16", "float32"];

/// What the checkpoint manifest (ADR 0014 §7, WE3) tells resolution. Absent
/// facts never block a deployment that declares its memory in full.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CheckpointFacts {
    /// Sum of the checkpoint's weight-file sizes.
    pub weights_bytes: Option<i64>,
    /// A snapshot frozen before the startup memory budget existed records no
    /// startup peak; it is re-resolved exactly as it was, with a cold phase
    /// equal to the request. Never set for a new resolution.
    pub legacy_startup: bool,
    /// A snapshot frozen before the engine's CUDA context and graphs were
    /// charged records no `overhead_bytes`; it re-resolves exactly as it was,
    /// without them. Never set for a new resolution.
    pub legacy_overhead: bool,
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
    accept_extra_args: Option<bool>,
    #[serde(default)]
    extra_args: Option<Vec<String>>,
}

impl RawEngineConfig {
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
            "cannot size the device request: the checkpoint's weight size is not known yet \
             (ADR 0014 §5, §7); it is sized once the checkpoint is measured",
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
        Engine::Sglang => 0,
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
             free memory, which SPEC §7.1 forbids (ADR 0014 §5)",
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
                     known yet (ADR 0014 §5, §7); declare memory.request",
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
                     yet (ADR 0014 §5, §7); declare memory.kv_cache",
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
        },
        derived,
    ))
}

/// Owner decision 2026-09-23 (startup memory budget): the startup peak and,
/// when mllm chose it, its provenance.
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
    let peak = default_startup_bytes(memory.request_bytes, memory.weights_bytes, margin)
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
                 is not known yet (ADR 0014 §5, §7)",
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
    // phases it joins, `mllm_domain::resources::validate_recipe`).
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
    match engine {
        Engine::Vllm if raw.sglang.is_some() => return Err(family_mismatch("sglang")),
        Engine::Sglang if raw.vllm.is_some() => return Err(family_mismatch("vllm")),
        _ => {}
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
    let declared: [(&str, bool); 13] = [
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
        ("sglang.max_total_tokens", sglang.max_total_tokens.is_some()),
        (
            "sglang.chunked_prefill_size",
            sglang.chunked_prefill_size.is_some(),
        ),
        (
            "sglang.tokenizer_workers",
            sglang.tokenizer_workers.is_some(),
        ),
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
                "extra engine arguments need engine_config.accept_extra_args: true (ADR 0014 §6)",
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

    let raw_memory = raw.memory.clone().unwrap_or_default();
    let declared_request = raw_memory.request.as_deref().map(parse_bytes).transpose()?;
    let kv_cache = raw_memory
        .kv_cache
        .as_deref()
        .map(parse_bytes)
        .transpose()?;
    let mut declared_startup = raw_memory.startup.as_deref().map(parse_bytes).transpose()?;
    // Review decision (discrete GPU design §3): a request derived from the
    // weights on a device domain is sized for the card, not with the unified
    // placeholder margin, which would not fit a small card.
    let device_request = match (inputs.device, declared_request, inputs.declared_ready_total) {
        (Some(device), None, None) => {
            device_request_from_weights(device, engine, inputs.facts.weights_bytes, kv_cache)?
        }
        _ => None,
    };
    let (mut memory, memory_provenance) = resolve_memory(MemoryInputs {
        request: device_request.or(declared_request),
        kv_cache,
        declared_ready_total: inputs.declared_ready_total,
        weights: inputs.facts.weights_bytes,
        margin: overhead_margin(engine),
    })?;
    let mut provenance: BTreeMap<String, SettingSource> = memory_provenance
        .into_iter()
        .map(|(field, source)| (field.to_owned(), source))
        .collect();
    if let Some(request) = device_request {
        provenance.insert("memory.request".into(), SettingSource::Derived);
        // Design §3: the engine's use of the card is bounded by the fraction
        // mllm renders from this request, so the device peak is the request.
        if declared_startup.is_none() {
            declared_startup = Some(request);
            provenance.insert("memory.startup".into(), SettingSource::Derived);
        }
    }
    let (startup, startup_source) = resolve_startup(
        declared_startup,
        &memory,
        inputs.declared_ready_total.is_some(),
        inputs.facts,
        overhead_margin(engine),
    )?;
    memory.startup_bytes = startup;
    if let Some(source) = startup_source {
        provenance.insert("memory.startup".into(), source);
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
            let weight_restore = if cpu_weight_backup {
                "cpu_backup"
            } else {
                "disk_reload"
            };
            for field in ["memory_saver", "cpu_weight_backup", "weight_restore"] {
                provenance.insert(field.into(), SettingSource::Derived);
            }
            // ADR 0014 §4: a live finding, not a checkpoint pin. CUDA graphs stay
            // off while the memory saver is on unless the deployment says otherwise.
            if common.cuda_graphs.is_none() && memory_saver {
                common.cuda_graphs = Some(false);
                provenance.insert("cuda_graphs".into(), SettingSource::MllmDefault);
            }
            let tokenizer_workers = match sglang.tokenizer_workers {
                Some(workers) => workers,
                None => {
                    provenance.insert(
                        "sglang.tokenizer_workers".into(),
                        SettingSource::MllmDefault,
                    );
                    1
                }
            };
            LaunchSettings::Sglang(SglangLaunchSettings {
                common,
                memory,
                max_total_tokens: sglang.max_total_tokens,
                chunked_prefill_size: sglang.chunked_prefill_size,
                tokenizer_workers,
                memory_saver,
                cpu_weight_backup,
                weight_restore: weight_restore.into(),
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
    Ok(serde_json::json!({
        "dtype": raw.dtype, "quantization": raw.quantization,
        "kv_cache_dtype": raw.kv_cache_dtype, "context_length": raw.context_length,
        "max_concurrent_requests": raw.max_concurrent_requests,
        "cuda_graphs": raw.cuda_graphs, "language_model_only": raw.language_model_only,
        "trust_remote_code": raw.trust_remote_code,
        "memory": declared_memory(&memory)?,
        "vllm": raw.vllm.as_ref().map(|v| serde_json::json!({
            "block_size_tokens": v.block_size_tokens,
            "max_num_batched_tokens": v.max_num_batched_tokens,
        })),
        "sglang": raw.sglang.as_ref().map(|s| serde_json::json!({
            "max_total_tokens": s.max_total_tokens,
            "chunked_prefill_size": s.chunked_prefill_size,
            "tokenizer_workers": s.tokenizer_workers,
        })),
        "accept_extra_args": raw.accept_extra_args,
        "extra_args": raw.extra_args,
    }))
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
