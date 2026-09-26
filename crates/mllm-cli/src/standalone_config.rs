//! The host policy and deployment document standalone publishes.
//!
//! The profile is ADR 0008's engine installation, and it is now the installation the
//! provider actually found rather than a shape invented here. Limits derive from
//! observed capacity rather than a configured guess, because an invented ceiling is
//! how a host gets overcommitted.

use crate::device_inventory::InventoryPublication;
use mllm_agent::gpu_memory::{GpuMemory, HostShape};
use mllm_config::effective::ModelSource;
use mllm_config::engine_policy::Engine;
use mllm_controller::engine_provider::NamedInstallation;
use mllm_controller::EngineInstallation;
use serde_json::{json, Value};

/// The profile an environment variable's installation is published under when
/// only one is set (ADR 0018 §5).
pub const STANDALONE_PROFILE: &str = "local";

/// Unified: on this hardware device and host memory are one physical pool.
const DOMAIN: &str = "unified";

/// Conservative: admission must fail before the host does.
const MANAGED_FRACTION: i64 = 50;
const FREE_RESERVE_FRACTION: i64 = 20;
const PARKED_FRACTION: i64 = 25;
const HOST_KV_FRACTION: i64 = 10;
/// ADR 0014 §5: the default KV cache, below the Ready allocation (15%).
const KV_CACHE_FRACTION: i64 = 10;
/// The most engines the host keeps parked at once.
const MAX_PARKED: i64 = 4;

/// A discrete device domain's limits (design §2, "Standalone defaults").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceLimits {
    pub managed_limit: i64,
    pub free_reserve: i64,
    pub parked_limit: i64,
}

/// ADR 0019: the device reserve absorbs a display server's use — the larger of
/// 1 GiB and 8 % of the card — and everything above it is managed. Parked
/// engines leave a CUDA context on the card, bounded at 2 GiB for each engine
/// that may be parked and at most a quarter of the card.
pub fn device_limits(memory: &GpuMemory, max_parked: i64) -> DeviceLimits {
    const GIB: i64 = 1 << 30;
    let total = memory.total_bytes;
    let free_reserve = (total / 100 * 8).max(GIB);
    DeviceLimits {
        managed_limit: total - free_reserve,
        free_reserve,
        parked_limit: (2 * GIB * max_parked).min(total / 100 * 25),
    }
}

/// The name the published table uses for an engine family.
fn engine_name(engine: Engine) -> &'static str {
    match engine {
        Engine::Vllm => "vllm",
        Engine::Sglang => "sglang",
    }
}

/// One installation's published runtime profile.
///
/// SPEC §13.3, §9.1 / T21, ADR 0012: every launch seals two per-launch keys
/// under distinct roles, inference and admin, so the profile names both
/// references; the coordinator resolves them per launch, never from the
/// profile itself. A vLLM launch keys its development and control routes with
/// the admin key, apart from the inference key ingress holds.
fn runtime_profile(installation: &EngineInstallation) -> Value {
    let mut security = json!({
        "deep_park": if installation.deep_park { "enabled" } else { "disabled" },
        "trust_remote_code": installation.trust_remote_code,
        "credential_ref": "secret://engine-key",
        "admin_credential_ref": "secret://admin-key"
    });
    // ADR 0008 (owner decision 2026-09-23): stated only when the host refuses
    // drift, so a default document is unchanged.
    if installation.installation_drift == mllm_config::effective::InstallationDrift::Refuse {
        security["installation_drift"] = json!("refuse");
    }
    let mut profile = json!({
        "engine": engine_name(installation.engine),
        "revision": 1,
        "executable": installation.executable.to_string_lossy(),
        "build_fingerprint": installation.build_fingerprint,
        "args": installation.args,
        "env": {},
        "log_policy": {"max_file_bytes": "16MiB", "retained_files": 3},
        "security": security
    });
    // SPEC §13.3 amendment (owner decision 2026-09-25): stated only when the
    // installation names one, so a default document is unchanged.
    if let Some(cuda_home) = &installation.cuda_home {
        profile["cuda_home"] = json!(cuda_home.to_string_lossy());
    }
    profile
}

/// The host policy standalone publishes: the engine installations it offers (ADR
/// 0018 §5: the environment's and the registered ones, one profile each), the
/// store its weights live under, and the limits it will admit against.
///
/// Spec §7: the published table states the flags the installation's profile
/// passes, whether deep park is available on it, and where models are kept. ADR
/// 0014 §1: engine tuning is the deployment's, so no launch settings are published. Every one of those comes from the installation the provider found, so
/// what is published and what would be launched cannot disagree.
///
/// `environment_fingerprint` names the surrounding environment the installation was
/// found in; `capacity_bytes` is the host's observed total, not a configured guess.
///
/// `inventory` is the host's NVIDIA device publication, observed at boot by the
/// bounded collector (`crate::device_inventory`). The digest rides the host
/// document as `device_inventory_digest`; each device's corroborated physical
/// UUID rides its `gpuN` entry, which is what the guarded launcher sets the
/// engine child's `CUDA_VISIBLE_DEVICES` from. A host with no inventory
/// publishes neither — placement then fails closed at the native gate,
/// honestly, rather than here.
///
/// `shape` is the host's GPU shape, sampled once at boot (design §1). A unified
/// host, and a host with no GPU, publish the single `unified` domain exactly as
/// before. ADR 0019: a discrete host publishes a `system` domain for host RAM
/// and one `device` domain per GPU, named after its device `gpuN`.
pub fn host_policy(
    installations: &[NamedInstallation],
    environment_fingerprint: &str,
    capacity_bytes: i64,
    inventory: Option<&InventoryPublication>,
    shape: &HostShape,
) -> Value {
    let share = |percent: i64| format!("{}B", capacity_bytes / 100 * percent);
    // ADR 0018 §5: role-level fields (model store, ports, hardware
    // fingerprint) come from the first installation; every installation is
    // one profile. The provider never returns an empty list.
    let first = &installations
        .first()
        .expect("a standalone host publishes at least one installation")
        .installation;
    let (domains, devices) = match shape {
        HostShape::Unified | HostShape::NoGpu => {
            let mut gpu0 = json!({"domain": DOMAIN, "sharing": "shared"});
            // The single entry names the inventory's one device, whatever
            // index the driver gave it.
            if let Some(published) = inventory {
                if let [uuid] = published.physical_gpu_uuids.values().collect::<Vec<_>>()[..] {
                    gpu0["physical_gpu_uuid"] = json!(uuid);
                }
            }
            let domains = json!({
                DOMAIN: {
                    "managed_limit": share(MANAGED_FRACTION),
                    "free_reserve": share(FREE_RESERVE_FRACTION),
                    "parked_limit": share(PARKED_FRACTION),
                    "host_kv_limit": share(HOST_KV_FRACTION),
                    // One physical pool: a weight backup "in host RAM" would
                    // allocate from the memory it is meant to free.
                    "memory": "unified"
                }
            });
            (domains, json!({"gpu0": gpu0}))
        }
        HostShape::Discrete(gpus) => {
            let mut domains = serde_json::Map::new();
            domains.insert(
                "system".into(),
                json!({
                    "managed_limit": share(MANAGED_FRACTION),
                    "free_reserve": share(FREE_RESERVE_FRACTION),
                    "parked_limit": share(PARKED_FRACTION),
                    "host_kv_limit": share(HOST_KV_FRACTION),
                    // ADR 0019: host RAM only; the GPU has its own domain.
                    "memory": "distinct"
                }),
            );
            let mut devices = serde_json::Map::new();
            for gpu in gpus {
                let id = format!("gpu{}", gpu.index);
                // `HostShape::Discrete` holds only devices with memory of their own.
                let memory = gpu
                    .memory
                    .as_ref()
                    .expect("a discrete device reports its memory");
                let limits = device_limits(memory, MAX_PARKED);
                domains.insert(
                    id.clone(),
                    json!({
                        // ADR 0019: this device's VRAM; host-KV lives in RAM,
                        // so a device domain states no `host_kv_limit`.
                        "memory": "device",
                        "device": id,
                        "managed_limit": format!("{}B", limits.managed_limit),
                        "free_reserve": format!("{}B", limits.free_reserve),
                        "parked_limit": format!("{}B", limits.parked_limit)
                    }),
                );
                let mut entry = json!({"domain": id, "sharing": "shared"});
                if let Some(uuid) =
                    inventory.and_then(|published| published.physical_gpu_uuids.get(&gpu.index))
                {
                    entry["physical_gpu_uuid"] = json!(uuid);
                }
                devices.insert(id, entry);
            }
            (Value::Object(domains), Value::Object(devices))
        }
    };
    let profiles: serde_json::Map<String, Value> = installations
        .iter()
        .map(|named| (named.profile.clone(), runtime_profile(&named.installation)))
        .collect();
    json!({
        "schema_version": 1,
        "kind": "host",
        "name": inventory.map_or("standalone", |published| published.host_id.as_str()),
        "hardware_fingerprint": format!("standalone-{}", engine_name(first.engine)),
        "environment_fingerprint": environment_fingerprint,
        // SPEC §3: the versioned NVIDIA inventory digest is placement evidence
        // the native launch asserts against. Absent (null) when the host
        // observed no inventory, which normalizes back to `None`.
        "device_inventory_digest": inventory.map(|published| published.digest.clone()),
        // Spec §7: a relative model path resolves against this, so the host states
        // it rather than having a directory guessed for it.
        "model_store": {"path": first.models_root.to_string_lossy()},
        "runtime_profiles": profiles,
        "resource_policy": {
            "domains": domains,
            "devices": devices,
            "device_sharing": "shared",
            "max_parked": MAX_PARKED,
            "observation_ttl": "2s",
            "endpoint_port_range": {
                "start": first.engine_ports.0,
                "end": first.engine_ports.1
            },
            "planner_max_states": 4096,
            "queue": {
                "admission_window": "2s",
                "max_pending_per_deployment": 64,
                "max_pending_total": 256,
                "max_buffered_bytes_total": "64MiB",
                "request_deadline": "1800s"
            }
        }
    })
}

/// The request deadline a deployment carries unless its caller names another.
///
/// It bounds how far ahead an operation's deadline may be set, so it has to be at
/// least the window the coordinator gives an activation; a shorter one makes a start
/// inadmissible rather than merely impatient.
pub const DEFAULT_REQUEST_DEADLINE: &str = "900s";

/// The deployment document, naming the installation it runs on and where its
/// weights come from.
///
/// Spec §7: the model is stated as a source rather than a bare path, so a fetched
/// checkpoint is expressible in the same document that a local one is.
///
/// Phase footprints are declared because admission compares a transition's peak
/// against the ceiling, not its steady state.
///
/// ADR 0014 §2, §5: on a unified host ([`TemplateMemory::Unified`]) the
/// document carries an `engine_config` whose KV cache is a tenth of capacity,
/// inside the Ready allocation that is its memory request. Standalone replaces
/// it with the installation's configured block
/// ([`EngineInstallation::engine_config`]) when it deploys. That document is
/// byte-identical to the one before discrete hosts had a template.
///
/// Design §3: on a discrete host ([`TemplateMemory::Device`]) the document
/// states a device memory request sized from the checkpoint's weights
/// ([`device_request`]) and omits `resources:`, so its phases derive on the GPU
/// the picker chooses; the residency is [`default_residency`]. A request the
/// device domain cannot hold is refused
/// ([`TemplateError::InsufficientDeviceMemory`]).
///
/// `request_deadline` is a parameter rather than a constant because the deadline is
/// a property of the deployment an operator asks for, and the live suite has to be
/// able to state a short one to see what the bound does. Ordinary callers pass
/// [`DEFAULT_REQUEST_DEADLINE`].
///
/// `deep_park` is the host's switch ([`EngineInstallation::deep_park`]). ADR 0012:
/// deep parking is on by default and a host opts out, so the generated residency
/// has to follow that switch rather than state a tier the host's profile refuses.
/// The residency does not depend on the engine (vLLM sleeps, SGLang uses its
/// memory saver). The engine decides only a discrete request's floor (vLLM's).
///
/// `profile` is the runtime profile the deployment runs on (ADR 0018 §5: the
/// host may publish several; [`STANDALONE_PROFILE`] is the environment's).
// Each argument is a separate field of the document.
#[allow(clippy::too_many_arguments)]
pub fn deployment_document(
    name: &str,
    route: &str,
    source: &ModelSource,
    engine: Engine,
    memory: &TemplateMemory,
    request_deadline: &str,
    deep_park: bool,
    profile: &str,
) -> Result<Value, TemplateError> {
    let capacity_bytes = match *memory {
        TemplateMemory::Unified { capacity_bytes } => capacity_bytes,
        TemplateMemory::Device {
            managed_limit,
            device_total,
            weights_bytes,
            system_parked_limit,
            kv_cache_bytes,
        } => {
            return discrete_document(
                name,
                route,
                source,
                engine,
                DiscreteTemplate {
                    managed_limit,
                    device_total,
                    weights_bytes,
                    system_parked_limit,
                    kv_cache_bytes,
                },
                request_deadline,
                deep_park,
                profile,
            )
        }
    };
    let share = |percent: i64| format!("{}B", capacity_bytes / 100 * percent);
    let devices = json!([{"id": "gpu0", "sharing": "shared"}]);
    // ADR 0012: deep parking is on by default and a host opts out, and the
    // generated residency follows that switch for every engine, so standalone
    // parks exactly as server mode does. SPEC §6.2: a deep-parking host
    // declares `deep`; engine configuration resolution then derives vLLM's
    // sleep mode and SGLang's memory saver from the declared residency (ADR
    // 0014 §4). A restart_only vLLM deployment would launch without sleep mode
    // and never park, so idle eviction and switching would stop it cold.
    // SPEC §6.2: restart_only is a first-class residency, not a failure mode;
    // an opted-out host declares it, launches without sleep mode or the memory
    // saver, and its park is refused `unchanged`. A `deep` deployment on an
    // opted-out host would be refused at resolution (T21). ADR 0008: a build
    // whose probe finds deep parking missing refuses the deep launch
    // `capability_missing:deep_park`, and the host falls back by opting out
    // (MLLM_DEEP_PARK=off), which declares restart_only here.
    let residency = if deep_park { "deep" } else { "restart_only" };
    let allocation = |percent: i64, kv: i64| json!([{"domain": DOMAIN, "bytes": share(percent), "host_kv_bytes": share(kv)}]);
    Ok(json!({
        "schema_version": 1,
        "kind": "deployment",
        "name": name,
        "routes": [route],
        // ADR 0008 calls this an engine installation; the schema key still says
        // runtime_profile, and the rename is tracked there.
        "runtime_profile": profile,
        "runtime_profile_revision": 1,
        "recipe": "standalone",
        // ADR 0010 makes the residency declarable; the template states the tier
        // the host's profile can deliver (see `residency` above).
        "residency": residency,
        "recovery": "reconcile",
        // Ordered: activation window <= deployment deadline <= host ceiling.
        "request_deadline": request_deadline,
        "model": {
            "source": source,
            "content_fingerprint": format!("sha256:{name}"),
            "revision": "r1"
        },
        "devices": devices,
        "engine_config": {"memory": {"kv_cache": share(KV_CACHE_FRACTION)}},
        "resources": {
            "cold":    {"allocations": allocation(20, 2), "devices": devices},
            "ready":   {"allocations": allocation(15, 2), "devices": devices},
            "parking": {"allocations": allocation(15, 2), "devices": devices},
            "parked":  {"allocations": allocation(2, 0),  "devices": []},
            "wake":    {"allocations": allocation(20, 2), "devices": devices}
        }
    }))
}

/// What the deployment template is sized from.
///
/// Design §3: a unified host states fixed shares of its observed capacity; a
/// discrete host states a memory request sized from the checkpoint's weights,
/// and its phases derive from it. There is no device id: the picker chooses the
/// GPU (discrete GPU design §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateMemory {
    Unified {
        capacity_bytes: i64,
    },
    Device {
        /// The device domain's managed limit ([`device_limits`]).
        managed_limit: i64,
        /// The card's total memory, which vLLM's floor is a fraction of.
        device_total: i64,
        /// ADR 0014 §5: the sum of the checkpoint's weight-file sizes. `None`
        /// for a Hugging Face or HTTP source, whose weights are known only once
        /// downloaded (review decision): the template then states the KV
        /// cache alone and resolution sizes the request once the checkpoint
        /// digest measures the weights (ADR 0014 §7).
        weights_bytes: Option<i64>,
        /// What the system domain holds parked: the smaller of its
        /// `parked_limit` and `managed_limit`.
        system_parked_limit: i64,
        /// The KV cache the operator stated (`MLLM_KV_CACHE_BYTES`), honoured
        /// within the card or refused (review decision). `None`: sized from
        /// the card.
        kv_cache_bytes: Option<i64>,
    },
}

/// Why no deployment template could be generated.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    /// Spec §3, §11: the device request exceeds the device domain's managed
    /// limit, so no placement could ever hold it.
    #[error(
        "insufficient_device_memory: the deployment needs a device memory request of {request} \
         bytes (weights x 1.10 plus the KV cache), above the {limit} bytes the device domain \
         manages; use a smaller or quantized checkpoint"
    )]
    InsufficientDeviceMemory { request: i64, limit: i64 },
    /// Review decision: the KV cache the operator stated cannot fit the card
    /// with the checkpoint's weights (or alone), so it is refused rather than
    /// silently replaced.
    #[error(
        "insufficient_device_memory: MLLM_KV_CACHE_BYTES asks for a {kv}-byte KV cache, which \
         makes a device memory request of {request} bytes, above the {limit} bytes the device \
         domain manages; lower MLLM_KV_CACHE_BYTES or unset it to size the KV cache from the card"
    )]
    DeclaredKvCacheTooLarge { kv: i64, request: i64, limit: i64 },
}

impl TemplateError {
    /// The structured error code the refusal is reported under.
    pub fn code(&self) -> &'static str {
        match self {
            TemplateError::InsufficientDeviceMemory { .. }
            | TemplateError::DeclaredKvCacheTooLarge { .. } => "insufficient_device_memory",
        }
    }
}

/// Spec §3: the device memory request and KV cache of a discrete deployment.
///
/// The KV cache is `min(4 GiB, managed_limit / 4)` and the request is
/// `weights x 1.10 + kv`. vLLM's request is at least 0.75 of the card.
pub fn device_request(
    engine: Engine,
    weights_bytes: i64,
    managed_limit: i64,
    device_total: i64,
) -> (i64, i64) {
    device_request_with_kv(engine, weights_bytes, managed_limit, device_total, None)
}

/// The default KV cache of a discrete template: `min(4 GiB, managed_limit / 4)`,
/// the rule every deployment that states no memory gets (owner decision
/// 2026-09-25, `mllm_config::deployment_defaults::default_kv_cache`).
pub fn default_device_kv(managed_limit: i64) -> i64 {
    mllm_config::deployment_defaults::default_kv_cache(managed_limit)
}

/// [`device_request`] with the KV cache the operator stated, when stated.
pub fn device_request_with_kv(
    engine: Engine,
    weights_bytes: i64,
    managed_limit: i64,
    device_total: i64,
    kv_cache_bytes: Option<i64>,
) -> (i64, i64) {
    let kv = kv_cache_bytes.unwrap_or_else(|| default_device_kv(managed_limit));
    let request = (weights_bytes / 100).saturating_mul(110).saturating_add(kv);
    // Spec §3: vLLM 0.29 with CUDA graphs starts a 4B model on a 16 GB card
    // only at --gpu-memory-utilization >= 0.75.
    let floor = if engine == Engine::Vllm {
        device_total / 100 * 75
    } else {
        0
    };
    (request.max(floor), kv)
}

/// The residency the generated template declares.
///
/// ADR 0012: `restart_only` when the host opted out of deep parking. Spec §5
/// (owner decision 2): on a discrete host a wake from host RAM is the default
/// when the copy fits what the system domain holds parked; otherwise `deep`.
/// `deep` on a unified host, where a copy in host RAM frees nothing (ADR 0010).
///
/// `discrete` is `(weights, system parked limit)`. The parked system
/// allocation is the engine's host overhead plus the copy (design §3), which is
/// what resolution holds to the limit (`host_backed_unavailable`), so the
/// overhead is counted here too: the template never states a tier its own
/// resolution refuses.
///
/// The rule is the one every deployment that states no residency gets (owner
/// decision 2026-09-25, `mllm_config::deployment_defaults::default_residency`).
pub fn default_residency(deep_park: bool, discrete: Option<(i64, i64)>) -> &'static str {
    use mllm_config::effective::Residency;
    match mllm_config::deployment_defaults::default_residency(
        deep_park,
        discrete.map(|(weights, parked_limit)| (Some(weights), parked_limit)),
    ) {
        Residency::RestartOnly => "restart_only",
        Residency::HostBacked => "host_backed",
        Residency::Deep => "deep",
    }
}

/// Design §3: the template memory of a discrete host, sized on its largest GPU
/// (the one most likely to hold the deployment; the picker still chooses) and
/// the system domain's parked room. `None` when there is no GPU.
pub fn discrete_template_memory(
    gpus: &[mllm_agent::gpu_memory::GpuDevice],
    capacity_bytes: i64,
    weights_bytes: Option<i64>,
    kv_cache_bytes: Option<i64>,
) -> Option<TemplateMemory> {
    let largest = gpus
        .iter()
        .filter_map(|gpu| gpu.memory.as_ref())
        .max_by_key(|memory| memory.total_bytes)?;
    let limits = device_limits(largest, MAX_PARKED);
    // The same shares `host_policy` publishes for the system domain; the
    // parked copy is held to the smaller of its parked and managed limits.
    let system_parked_limit =
        (capacity_bytes / 100 * PARKED_FRACTION).min(capacity_bytes / 100 * MANAGED_FRACTION);
    Some(TemplateMemory::Device {
        managed_limit: limits.managed_limit,
        device_total: largest.total_bytes,
        weights_bytes,
        system_parked_limit,
        kv_cache_bytes,
    })
}

struct DiscreteTemplate {
    managed_limit: i64,
    device_total: i64,
    weights_bytes: Option<i64>,
    system_parked_limit: i64,
    kv_cache_bytes: Option<i64>,
}

/// Design §3: the discrete template states the device memory request and omits
/// `resources:`, so every phase derives from the request as `[device, system]`
/// allocations on the GPU the picker chooses.
// Each argument is a separate field of the document.
#[allow(clippy::too_many_arguments)]
fn discrete_document(
    name: &str,
    route: &str,
    source: &ModelSource,
    engine: Engine,
    sizing: DiscreteTemplate,
    request_deadline: &str,
    deep_park: bool,
    profile: &str,
) -> Result<Value, TemplateError> {
    let too_large = |request: i64| match sizing.kv_cache_bytes {
        Some(kv) => TemplateError::DeclaredKvCacheTooLarge {
            kv,
            request,
            limit: sizing.managed_limit,
        },
        None => TemplateError::InsufficientDeviceMemory {
            request,
            limit: sizing.managed_limit,
        },
    };
    let Some(weights_bytes) = sizing.weights_bytes else {
        // Review decision: a Hugging Face or HTTP source. Its weights are
        // known only once downloaded, so the template states the KV cache alone
        // and resolution sizes the request (and the startup peak) from the
        // weights the checkpoint digest measures: acceptance freezes the
        // revision provisional until then (ADR 0014 §7). Without the weights
        // no host-RAM copy can be sized, so it parks deep.
        let kv = sizing
            .kv_cache_bytes
            .unwrap_or_else(|| default_device_kv(sizing.managed_limit));
        if kv > sizing.managed_limit {
            return Err(too_large(kv));
        }
        let residency = default_residency(deep_park, None);
        return Ok(discrete_json(
            name,
            route,
            source,
            profile,
            residency,
            request_deadline,
            json!({"kv_cache": format!("{kv}B")}),
        ));
    };
    let (request, kv) = device_request_with_kv(
        engine,
        weights_bytes,
        sizing.managed_limit,
        sizing.device_total,
        sizing.kv_cache_bytes,
    );
    // Spec §3: a request the device domain can never hold is refused at
    // deploy, with the numbers, before anything is stored. A KV cache the
    // operator stated is named in the refusal (review decision).
    // ADR 0019: the device domain is charged the request and the engine's
    // CUDA context and graphs, so the template refuses what resolution would.
    let charged =
        request.saturating_add(mllm_config::effective::ENGINE_DEVICE_OVERHEAD_PLACEHOLDER_BYTES);
    if charged > sizing.managed_limit {
        return Err(too_large(charged));
    }
    let residency = default_residency(deep_park, Some((weights_bytes, sizing.system_parked_limit)));
    Ok(discrete_json(
        name,
        route,
        source,
        profile,
        residency,
        request_deadline,
        json!({
            "request": format!("{request}B"),
            "kv_cache": format!("{kv}B"),
            // Design §3: the device holds the startup peak in the cold phase.
            // The engine's use of the card is bounded by the fraction mllm
            // renders from this request, so the device peak is the request; the
            // unified placeholder (weights x 1.6 plus a margin) models load
            // buffers in the one pool, which on a discrete host sit in host RAM,
            // and would size a 4B model beyond a 16 GB card.
            "startup": format!("{request}B")
        }),
    ))
}

/// The discrete deployment document around its `engine_config.memory` block.
fn discrete_json(
    name: &str,
    route: &str,
    source: &ModelSource,
    profile: &str,
    residency: &str,
    request_deadline: &str,
    memory: Value,
) -> Value {
    json!({
        "schema_version": 1,
        "kind": "deployment",
        "name": name,
        "routes": [route],
        "runtime_profile": profile,
        "runtime_profile_revision": 1,
        "recipe": "standalone",
        "residency": residency,
        "recovery": "reconcile",
        "request_deadline": request_deadline,
        "model": {
            "source": source,
            "content_fingerprint": format!("sha256:{name}"),
            "revision": "r1"
        },
        // Discrete GPU design §7: no device is pinned; placement picks the GPU.
        // The key is required by the deployment schema.
        "devices": [],
        "engine_config": {"memory": memory}
    })
}

#[cfg(test)]
mod tests;
