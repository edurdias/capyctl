//! What standalone mode declares about itself.
//!
//! The coordinator starts only a deployment it can qualify, and qualification is
//! against an effective configuration: a host policy naming the engine installations
//! this host offers, and a deployment naming the one it wants. Standalone previously
//! declared no profiles at all, so nothing could be qualified — it hardcoded an
//! adapter in the role wiring and never told the store the engine existed.
//!
//! These compose that declaration. The profile is the engine installation from
//! ADR 0008: an engine family, an executable, a build fingerprint, and the security
//! decisions the host has made about it.
//!
//! Resource limits are derived from what the host actually reports rather than
//! guessed, because an invented ceiling is how a machine gets overcommitted.

use serde_json::{json, Value};

/// The single profile standalone registers. Named rather than anonymous so a second
/// installation can be added later without the first becoming ambiguous.
pub const STANDALONE_PROFILE: &str = "local";

/// The domain a standalone host accounts in. Unified is correct for the Spark-class
/// hardware this runs on, where device and host memory are one physical pool.
const DOMAIN: &str = "unified";

/// Fractions of observed capacity. Conservative on purpose: admission must fail
/// before the host does, and a standalone host is also running everything else the
/// user is doing.
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
                // Standalone issues no qualification of its own: this names the
                // evidence the profile was admitted under, and an unqualified
                // profile is refused for warm use rather than silently downgraded.
                "qualification_id": format!("standalone-{build_fingerprint}"),
                "args": [],
                "env": {},
                "launch_settings": {"engine": engine},
                "log_policy": {"max_file_bytes": "16MiB", "retained_files": 3},
                "security": {
                    // The host's decision, not the adapter's: deep-park paths stay
                    // denied unless this says otherwise (SPEC §9.1, T21).
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
                    "host_kv_limit": share(HOST_KV_FRACTION)
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
                "request_deadline": "600s"
            }
        }
    })
}

/// The deployment document for a request, naming the profile it runs on.
///
/// Phase footprints are declared rather than inferred. A deployment that does not
/// state what it needs at each phase cannot be admitted, because admission compares
/// the transition's true peak against the ceiling rather than its steady state.
pub fn deployment_document(name: &str, route: &str, model_path: &str, capacity_bytes: i64) -> Value {
    let share = |percent: i64| format!("{}B", capacity_bytes / 100 * percent);
    let devices = json!([{"id": "gpu0", "sharing": "shared"}]);
    let allocation = |percent: i64, kv: i64| {
        json!([{"domain": DOMAIN, "bytes": share(percent), "host_kv_bytes": share(kv)}])
    };
    json!({
        "kind": "deployment",
        "name": name,
        "routes": [route],
        "recipe": "standalone",
        "residency": "warm",
        "recovery": "reconcile",
        "request_deadline": "300s",
        "profile": STANDALONE_PROFILE,
        "model": {
            "path": model_path,
            "content_fingerprint": format!("sha256:{name}"),
            "revision": "r1"
        },
        "devices": devices,
        "resources": {
            // Cold initialisation is the peak: loading costs more than serving.
            "cold":    {"allocations": allocation(20, 2), "devices": devices},
            "ready":   {"allocations": allocation(15, 2), "devices": devices},
            "parking": {"allocations": allocation(15, 2), "devices": devices},
            // Parked retains residue without holding a device.
            "parked":  {"allocations": allocation(2, 0),  "devices": []},
            "wake":    {"allocations": allocation(20, 2), "devices": devices}
        }
    })
}

#[cfg(test)]
mod tests;
