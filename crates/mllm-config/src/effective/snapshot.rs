use super::*;
use serde_json::{Value, json};

/// Decode and revalidate a bounded normalized snapshot without profile rereads.
pub fn decode_effective_snapshot(text: &str) -> Result<EffectiveDeployment, ConfigError> {
    if text.len() > 1 << 20 {
        return Err(invalid("snapshot", "snapshot exceeds 1MiB"));
    }
    // The strict walker rejects duplicate keys. Re-resolution below checks every
    // intrinsic rule; exact normalized equality rejects omitted/unknown fields,
    // changed fixed allocator settings, and untrusted fingerprint claims.
    let value = crate::strict_yaml::build_value(text)?;
    let mut resources = value["resources"].clone();
    for phase in ["cold", "ready", "parking", "parked", "wake"] {
        for allocation in resources[phase]["allocations"]
            .as_array_mut()
            .ok_or_else(|| invalid("snapshot.resources", "allocations required"))?
        {
            unit(allocation, "bytes", "bytes", "B")?;
            unit(allocation, "host_kv_bytes", "host_kv_bytes", "B")?;
        }
    }
    // `resolved_path` is derived from the source and the host's model store, so the
    // rebuilt deployment states the source only and resolution happens again. The
    // exact-equality check below is what proves the snapshot's claim matched.
    let mut model = value["model"].clone();
    model
        .as_object_mut()
        .ok_or_else(|| invalid("snapshot.model", "model required"))?
        .remove("resolved_path");
    let deployment = json!({
        "schema_version": value["schema_version"], "kind": "deployment",
        "name": value["name"], "model": model, "routes": value["routes"],
        "runtime_profile": "snapshot", "runtime_profile_revision": value["profile"]["revision"],
        "recipe": value["recipe"], "residency": value["residency"], "recovery": value["recovery"],
        "devices": value["selected_devices"], "resources": resources,
        "request_deadline": quantity(&value["request_deadline_ms"], "ms")?,
    });
    let mut profile = value["profile"].clone();
    unit(
        &mut profile["log_policy"],
        "max_file_bytes",
        "max_file_bytes",
        "B",
    )?;
    let settings = &mut profile["launch_settings"];
    match settings["engine"].as_str() {
        Some("fake") => {}
        Some("vllm") => {
            unit(settings, "cpu_offload_bytes", "cpu_offload_bytes", "B")?;
            unit(
                &mut settings["requested_budget"],
                "kv_cache_bytes",
                "kv_cache_bytes",
                "B",
            )?;
            unit(
                &mut settings["requested_budget"],
                "swap_space_bytes",
                "swap_space_bytes",
                "B",
            )?;
        }
        Some("sglang") => {
            let mut budget = settings["requested_budget"].clone();
            unit(&mut budget, "kv_cache_bytes", "kv_cache_bytes", "B")?;
            *settings =
                json!({"engine":"sglang", "recipe":settings["recipe"], "requested_budget":budget});
        }
        _ => return Err(invalid("snapshot.profile", "unsupported engine")),
    }
    let h = &value["host"];
    let mut domains = h["domains"].clone();
    for domain in domains
        .as_object_mut()
        .ok_or_else(|| invalid("snapshot.host", "domains required"))?
        .values_mut()
    {
        for field in [
            "managed_limit",
            "free_reserve",
            "host_kv_limit",
            "parked_limit",
        ] {
            if domain[field].is_null() && matches!(field, "host_kv_limit" | "parked_limit") {
                domain
                    .as_object_mut()
                    .ok_or_else(|| invalid("snapshot.host", "domain required"))?
                    .remove(field);
            } else {
                unit(domain, field, field, "B")?;
            }
        }
    }
    let mut queue = h["queue"].clone();
    unit(
        &mut queue,
        "max_buffered_bytes_total",
        "max_buffered_bytes_total",
        "B",
    )?;
    unit(&mut queue, "request_deadline_ms", "request_deadline", "ms")?;
    unit(&mut queue, "admission_window_ms", "admission_window", "ms")?;
    let host = json!({
        "schema_version": 1, "kind":"host", "name":h["name"],
        "hardware_fingerprint":h["hardware_fingerprint"], "environment_fingerprint":h["environment_fingerprint"],
        "model_store": {"path": h["model_store"]},
        "runtime_profiles":{"snapshot":profile},
        "resource_policy": {
            "domains":domains, "devices":h["devices"], "max_parked":h["max_parked"],
            "observation_ttl":quantity(&h["observation_ttl_ms"], "ms")?, "device_sharing":h["device_sharing"],
            "endpoint_port_range":h["endpoint_port_range"], "planner_max_states":h["planner_max_states"], "queue":queue,
        },
    });
    let effective = resolve_effective(&deployment, &host)?;
    if serde_json::to_value(&effective).map_err(|_| invalid("snapshot", "encoding failed"))?
        != value
    {
        return Err(invalid(
            "snapshot",
            "snapshot is not the exact validated normalized revision",
        ));
    }
    Ok(effective)
}

fn quantity(value: &Value, suffix: &str) -> Result<String, ConfigError> {
    value
        .as_i64()
        .filter(|n| *n >= 0)
        .map(|n| format!("{n}{suffix}"))
        .ok_or_else(|| invalid("snapshot", "nonnegative integral quantity required"))
}

fn unit(value: &mut Value, from: &str, to: &str, suffix: &str) -> Result<(), ConfigError> {
    let converted = quantity(&value[from], suffix)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| invalid("snapshot", "object required"))?;
    object.remove(from);
    object.insert(to.into(), json!(converted));
    Ok(())
}
