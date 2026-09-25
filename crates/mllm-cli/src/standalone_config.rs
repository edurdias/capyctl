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
    json!({
        "engine": engine_name(installation.engine),
        "revision": 1,
        "executable": installation.executable.to_string_lossy(),
        "build_fingerprint": installation.build_fingerprint,
        "args": installation.args,
        "env": {},
        "log_policy": {"max_file_bytes": "16MiB", "retained_files": 3},
        "security": security
    })
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
/// ADR 0014 §2, §5: the document carries an `engine_config` whose KV cache is a
/// tenth of capacity, inside the Ready allocation that is its memory request.
/// Standalone replaces it with the installation's configured block
/// ([`EngineInstallation::engine_config`]) when it deploys.
///
/// `request_deadline` is a parameter rather than a constant because the deadline is
/// a property of the deployment an operator asks for, and the live suite has to be
/// able to state a short one to see what the bound does. Ordinary callers pass
/// [`DEFAULT_REQUEST_DEADLINE`].
///
/// `deep_park` is the host's switch ([`EngineInstallation::deep_park`]). ADR 0012:
/// deep parking is on by default and a host opts out, so the generated residency
/// has to follow that switch rather than state a tier the host's profile refuses.
/// The residency no longer depends on the engine (vLLM sleeps, SGLang uses its
/// memory saver), so the engine argument does not decide it; it is kept so
/// callers name the installation the document is generated for.
///
/// `profile` is the runtime profile the deployment runs on (ADR 0018 §5: the
/// host may publish several; [`STANDALONE_PROFILE`] is the environment's).
// Each argument is a separate field of the document.
#[allow(clippy::too_many_arguments)]
pub fn deployment_document(
    name: &str,
    route: &str,
    source: &ModelSource,
    _engine: Engine,
    capacity_bytes: i64,
    request_deadline: &str,
    deep_park: bool,
    profile: &str,
) -> Value {
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
    json!({
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
    })
}

#[cfg(test)]
mod tests;
