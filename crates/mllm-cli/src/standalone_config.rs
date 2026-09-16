//! The host policy and deployment document standalone publishes.
//!
//! The profile is ADR 0008's engine installation. Limits derive from observed
//! capacity rather than a configured guess, because an invented ceiling is how a
//! host gets overcommitted.

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

/// The host policy standalone publishes, including the one engine installation it
/// offers and the limits it will admit against.
///
/// `capacity_bytes` is the host's observed total, not a configured guess.
pub fn host_policy(
    engine: &str,
    executable: &str,
    build_fingerprint: &str,
    experimental_controls: bool,
    capacity_bytes: i64,
) -> Value {
    let share = |percent: i64| format!("{}B", capacity_bytes / 100 * percent);
    json!({
        "schema_version": 1,
        "kind": "host",
        "name": "standalone",
        "hardware_fingerprint": format!("standalone-{engine}"),
        "environment_fingerprint": build_fingerprint,
        "runtime_profiles": {
            STANDALONE_PROFILE: {
                "engine": engine,
                "revision": 1,
                "executable": executable,
                "build_fingerprint": build_fingerprint,
                "qualification_id": format!("standalone-{build_fingerprint}"),
                "args": [],
                "env": {},
                "launch_settings": {"engine": engine},
                "log_policy": {"max_file_bytes": "16MiB", "retained_files": 3},
                "security": {
                    // SPEC §9.1/T21: the host's decision, not the adapter's.
                    "experimental_controls": experimental_controls,
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

/// The deployment document, naming the installation it runs on.
///
/// Phase footprints are declared because admission compares a transition's peak
/// against the ceiling, not its steady state.
pub fn deployment_document(name: &str, route: &str, model_path: &str, capacity_bytes: i64) -> Value {
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
            "path": model_path,
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
