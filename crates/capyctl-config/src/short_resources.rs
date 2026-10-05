//! The short form of a deployment's `resources` block.
//!
//! A model that restarts instead of parking holds the same memory from start
//! to stop, so its five phases repeat one figure. The short form states it
//! once:
//!
//! ```yaml
//! resources: {gpu: 11GiB, ram: 2GiB}
//! ```
//!
//! `gpu` is what the engine holds on the GPU, `ram` what its process holds in
//! host RAM. Where the document meets its host ([`expand_for_host`], called by
//! `deployment_defaults::for_host`), the short form becomes the five phases:
//! cold, ready, parking and wake charge both figures with the deployment's
//! one device; parked charges nothing and holds no device. On a discrete GPU
//! `gpu` charges the device's memory domain and `ram` the host's system
//! domain. On a unified machine both come from the one pool, so the pool is
//! charged their sum. The short form implies `residency: restart_only`; a
//! parking residency, or several devices, write the phases out.

use serde_json::{json, Map, Value};

use crate::effective::parse_bytes;
use crate::{ConfigError, ConfigErrorCode};

const PHASES: [&str; 5] = ["cold", "ready", "parking", "parked", "wake"];

fn invalid(path: &str, detail: impl Into<String>) -> ConfigError {
    ConfigError::new(ConfigErrorCode::UnsupportedCombination, path, detail)
}

/// Whether `resources` is written in the short form.
pub fn is_short(resources: &Value) -> bool {
    resources
        .as_object()
        .is_some_and(|block| block.contains_key("gpu") || block.contains_key("ram"))
}

/// The two figures of a short-form block, as written and in bytes, or `None`
/// for the long form. A mix of both forms or a missing figure is refused.
fn figures(resources: &Value) -> Result<Option<[(String, i64); 2]>, ConfigError> {
    let Some(block) = resources.as_object().filter(|_| is_short(resources)) else {
        return Ok(None);
    };
    if block.keys().any(|key| key != "gpu" && key != "ram") {
        return Err(invalid(
            "resources",
            "write either the short form `{gpu, ram}` or the five phases, not both",
        ));
    }
    let figure = |key: &str| -> Result<(String, i64), ConfigError> {
        let path = format!("resources.{key}");
        let text = block.get(key).and_then(Value::as_str).ok_or_else(|| {
            ConfigError::new(
                ConfigErrorCode::MissingRequired,
                &path,
                "the short form states both `gpu` and `ram`",
            )
        })?;
        let bytes = parse_bytes(text).map_err(|e| ConfigError::new(e.code, &path, e.detail))?;
        Ok((text.to_owned(), bytes))
    };
    let (gpu, ram) = (figure("gpu")?, figure("ram")?);
    if gpu.1 <= 0 {
        return Err(invalid("resources.gpu", "must be positive"));
    }
    if ram.1 < 0 {
        return Err(invalid("resources.ram", "must not be negative"));
    }
    Ok(Some([gpu, ram]))
}

/// The checks a short-form block needs no host for: its figures, a residency
/// that does not park and at most one device. `None` for the long form.
pub fn check(deployment: &Value) -> Result<Option<[(String, i64); 2]>, ConfigError> {
    let Some(figures) = figures(&deployment["resources"])? else {
        return Ok(None);
    };
    if let Some(residency) = deployment["residency"]
        .as_str()
        .filter(|r| *r != "restart_only")
    {
        return Err(invalid(
            "resources",
            format!(
                "the short form is for a model that restarts instead of parking; residency \
                 `{residency}` holds memory parked, so write the five phases"
            ),
        ));
    }
    if deployment["devices"]
        .as_array()
        .is_some_and(|d| d.len() > 1)
    {
        return Err(invalid(
            "resources",
            "the short form charges one device; with several devices write the five phases",
        ));
    }
    Ok(Some(figures))
}

/// What `validate config` shows for a short-form block without a host: the
/// five phases in the short form's own terms. `None` for the long form.
pub fn offline_phases(resources: &Value) -> Option<Value> {
    let [(gpu, _), (ram, _)] = figures(resources).ok()??;
    let active = json!({"gpu": gpu, "ram": ram});
    let mut phases = Map::new();
    for phase in PHASES {
        let value = if phase == "parked" {
            json!({"gpu": "0B", "ram": "0B"})
        } else {
            active.clone()
        };
        phases.insert(phase.into(), value);
    }
    Some(Value::Object(phases))
}

/// Replace a short-form block with the five phases it stands for on `host`.
/// A document whose one claim names no device yet keeps the short form until
/// placement names it (`instances::assign_devices`).
pub fn expand_for_host(deployment: &mut Value, host: &Value) -> Result<(), ConfigError> {
    let Some([(gpu_text, gpu), (ram_text, ram)]) = check(deployment)? else {
        return Ok(());
    };
    let claim = match deployment["devices"].as_array().map(Vec::as_slice) {
        Some([claim]) => claim.clone(),
        _ => {
            return Err(invalid(
                "resources",
                "the short form charges the deployment's one device, and none is selected",
            ))
        }
    };
    let Some(id) = claim["id"].as_str() else {
        return Ok(());
    };
    let policy = &host["resource_policy"];
    let domain = policy["devices"][id]["domain"]
        .as_str()
        .ok_or_else(|| invalid("devices", "unknown device"))?;
    let allocations = |gpu: Value, ram: Value| -> Result<Value, ConfigError> {
        if policy["domains"][domain]["memory"] != "device" {
            return Ok(json!([{"domain": domain, "bytes": gpu, "host_kv_bytes": "0B"}]));
        }
        let systems: Vec<&String> = policy["domains"]
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(_, d)| d["memory"] == "distinct")
            .map(|(name, _)| name)
            .collect();
        let [system] = systems.as_slice() else {
            return Err(invalid(
                "resource_policy.domains",
                "missing_system_allocation: a discrete host declares one distinct system domain \
                 for the short form's `ram`",
            ));
        };
        Ok(json!([
            {"domain": domain, "bytes": gpu, "host_kv_bytes": "0B"},
            {"domain": system, "bytes": ram, "host_kv_bytes": "0B"}
        ]))
    };
    let discrete = policy["domains"][domain]["memory"] == "device";
    let active = if discrete {
        allocations(json!(gpu_text), json!(ram_text))?
    } else {
        let sum = gpu
            .checked_add(ram)
            .ok_or_else(|| invalid("resources", "memory arithmetic overflows"))?;
        allocations(json!(bytes_text(sum)), Value::Null)?
    };
    let parked = allocations(json!("0B"), json!("0B"))?;
    let mut phases = Map::new();
    for phase in PHASES {
        let value = if phase == "parked" {
            json!({"allocations": parked, "devices": []})
        } else {
            json!({"allocations": active, "devices": [claim]})
        };
        phases.insert(phase.into(), value);
    }
    let object = deployment.as_object_mut().expect("a mapping has resources");
    object.insert("resources".into(), Value::Object(phases));
    object
        .entry("residency")
        .or_insert_with(|| json!("restart_only"));
    Ok(())
}

/// `bytes` in the largest binary unit that divides it.
fn bytes_text(bytes: i64) -> String {
    for (unit, size) in [
        ("TiB", 1_i64 << 40),
        ("GiB", 1 << 30),
        ("MiB", 1 << 20),
        ("KiB", 1 << 10),
    ] {
        if bytes != 0 && bytes % size == 0 {
            return format!("{}{unit}", bytes / size);
        }
    }
    format!("{bytes}B")
}
