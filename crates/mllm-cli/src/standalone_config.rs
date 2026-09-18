//! The host policy and deployment document standalone publishes.
//!
//! The profile is ADR 0008's engine installation, and it is now the installation the
//! provider actually found rather than a shape invented here. Limits derive from
//! observed capacity rather than a configured guess, because an invented ceiling is
//! how a host gets overcommitted.

use mllm_config::effective::ModelSource;
use mllm_config::engine_policy::Engine;
use mllm_controller::EngineInstallation;
use serde_json::{json, Value};

/// Named rather than anonymous so a second installation can be added later.
pub const STANDALONE_PROFILE: &str = "local";

/// Unified: on this hardware device and host memory are one physical pool.
const DOMAIN: &str = "unified";

/// Conservative: admission must fail before the host does.
const MANAGED_FRACTION: i64 = 50;
const FREE_RESERVE_FRACTION: i64 = 20;
const PARKED_FRACTION: i64 = 25;
const HOST_KV_FRACTION: i64 = 10;

/// The name the published table uses for an engine family.
fn engine_name(engine: Engine) -> &'static str {
    match engine {
        Engine::Vllm => "vllm",
        Engine::Sglang => "sglang",
    }
}

/// The host policy standalone publishes: the one engine installation it offers, the
/// store its weights live under, and the limits it will admit against.
///
/// Spec §7: the published table states the installation's own launch settings, the
/// flags its profile passes, whether deep park is available on it, and where models
/// are kept. Every one of those comes from the installation the provider found, so
/// what is published and what would be launched cannot disagree.
///
/// `environment_fingerprint` names the surrounding environment the installation was
/// found in; `capacity_bytes` is the host's observed total, not a configured guess.
pub fn host_policy(
    installation: &EngineInstallation,
    environment_fingerprint: &str,
    capacity_bytes: i64,
) -> Value {
    let share = |percent: i64| format!("{}B", capacity_bytes / 100 * percent);
    let engine = engine_name(installation.engine);
    json!({
        "schema_version": 1,
        "kind": "host",
        "name": "standalone",
        "hardware_fingerprint": format!("standalone-{engine}"),
        "environment_fingerprint": environment_fingerprint,
        // Spec §7: a relative model path resolves against this, so the host states
        // it rather than having a directory guessed for it.
        "model_store": {"path": installation.models_root.to_string_lossy()},
        "runtime_profiles": {
            STANDALONE_PROFILE: {
                "engine": engine,
                "revision": 1,
                "executable": installation.executable.to_string_lossy(),
                "build_fingerprint": installation.build_fingerprint,
                "args": installation.args,
                "env": {},
                "launch_settings": installation.launch_settings,
                "log_policy": {"max_file_bytes": "16MiB", "retained_files": 3},
                "security": {
                    // SPEC §9.1/T21: the host's decision, not the adapter's.
                    "deep_park": if installation.deep_park { "enabled" } else { "disabled" },
                    // Spec §3: executing checkpoint-supplied Python is opt-in.
                    "trust_remote_code": installation.trust_remote_code,
                    "credential_ref": "secret://engine-key"
                }
            }
        },
        "resource_policy": {
            "domains": {
                DOMAIN: {
                    "managed_limit": share(MANAGED_FRACTION),
                    "free_reserve": share(FREE_RESERVE_FRACTION),
                    "parked_limit": share(PARKED_FRACTION),
                    "host_kv_limit": share(HOST_KV_FRACTION),
                    // One physical pool: a weight backup "in host RAM" would
                    // allocate from the memory it is meant to free.
                    "memory": "unified"
                }
            },
            "devices": {"gpu0": {"domain": DOMAIN, "sharing": "shared"}},
            "device_sharing": "shared",
            "max_parked": 4,
            "observation_ttl": "2s",
            "endpoint_port_range": {"start": 8100, "end": 8199},
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

/// The deployment document, naming the installation it runs on and where its
/// weights come from.
///
/// Spec §7: the model is stated as a source rather than a bare path, so a fetched
/// checkpoint is expressible in the same document that a local one is.
///
/// Phase footprints are declared because admission compares a transition's peak
/// against the ceiling, not its steady state.
pub fn deployment_document(
    name: &str,
    route: &str,
    source: &ModelSource,
    capacity_bytes: i64,
) -> Value {
    let share = |percent: i64| format!("{}B", capacity_bytes / 100 * percent);
    let devices = json!([{"id": "gpu0", "sharing": "shared"}]);
    let allocation = |percent: i64, kv: i64| {
        json!([{"domain": DOMAIN, "bytes": share(percent), "host_kv_bytes": share(kv)}])
    };
    json!({
        "schema_version": 1,
        "kind": "deployment",
        "name": name,
        "routes": [route],
        // ADR 0008 calls this an engine installation; the schema key still says
        // runtime_profile, and the rename is tracked there.
        "runtime_profile": STANDALONE_PROFILE,
        "runtime_profile_revision": 1,
        "recipe": "standalone",
        // SPEC §6.2's fallback. Ordinary park is not implemented, so no parking tier
        // can be declared yet; ADR 0010 makes the choice expressible.
        "residency": "restart_only",
        "recovery": "reconcile",
        // Ordered: activation window <= deployment deadline <= host ceiling.
        "request_deadline": "900s",
        "model": {
            "source": source,
            "content_fingerprint": format!("sha256:{name}"),
            "revision": "r1"
        },
        "devices": devices,
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
