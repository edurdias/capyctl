use super::*;
use serde_json::{json, Value};

/// Decode and revalidate a bounded normalized snapshot without profile rereads.
pub fn decode_effective_snapshot(text: &str) -> Result<EffectiveDeployment, ConfigError> {
    if text.len() > 1 << 20 {
        return Err(invalid("snapshot", "snapshot exceeds 1MiB"));
    }
    // The strict walker rejects duplicate keys. Re-resolution below checks every
    // intrinsic rule; exact normalized equality rejects omitted/unknown fields,
    // changed fixed allocator settings, and untrusted fingerprint claims.
    let value = crate::strict_yaml::build_value(text)?;
    let (engine_config, resources_derived, facts) = declared_engine_config(&value["engine_config"])?;
    let (deployment, host) = snapshot_inputs(&value, engine_config, resources_derived)?;
    let effective = resolve_effective_with_checkpoint(&deployment, &host, facts)?;
    let mut encoded =
        serde_json::to_value(&effective).map_err(|_| invalid("snapshot", "encoding failed"))?;
    // ADR 0014 amendment A1: a revision frozen before `timeouts` existed has
    // none, and declared none. Its timeouts are derived again from the same
    // facts; everything else must still match exactly.
    if value.get("timeouts").is_none() {
        if let Some(object) = encoded.as_object_mut() {
            object.remove("timeouts");
        }
    }
    if encoded != value {
        return Err(invalid(
            "snapshot",
            "snapshot is not the exact validated normalized revision",
        ));
    }
    Ok(effective)
}

/// Rebuild the deployment and host documents a normalized snapshot was resolved
/// from, with `engine_config` as the deployment's declared block. Shared with the
/// pre-E1 upgrade (`legacy.rs`), which supplies a block mapped from the retired
/// profile `launch_settings` instead of one read from the snapshot.
pub(super) fn snapshot_inputs(
    value: &Value,
    engine_config: Value,
    resources_derived: bool,
) -> Result<(Value, Value), ConfigError> {
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
    let mut deployment = json!({
        "schema_version": value["schema_version"], "kind": "deployment",
        "name": value["name"], "model": model, "routes": value["routes"],
        "runtime_profile": "snapshot", "runtime_profile_revision": value["profile"]["revision"],
        "recipe": value["recipe"], "residency": value["residency"], "recovery": value["recovery"],
        "devices": value["selected_devices"], "resources": resources,
        "request_deadline": quantity(&value["request_deadline_ms"], "ms")?,
        "engine_config": engine_config,
    });
    // ADR 0014 §5: derived phases are re-derived rather than declared, so the
    // exact-equality check below proves the snapshot's claim.
    if resources_derived {
        deployment
            .as_object_mut()
            .expect("deployment is an object")
            .remove("resources");
    }
    // ADR 0014 amendment A1: only declared timeouts are restated; derived ones
    // are derived again, and the exact-equality check proves the claim.
    let mut timeouts = serde_json::Map::new();
    for (field, key) in [("initialize", "initialize_ms"), ("wake", "wake_ms")] {
        if value["timeouts"]["provenance"][field] == "declared" {
            timeouts.insert(field.into(), json!(quantity(&value["timeouts"][key], "ms")?));
        }
    }
    if !timeouts.is_empty() {
        deployment["timeouts"] = Value::Object(timeouts);
    }
    let mut profile = value["profile"].clone();
    unit(
        &mut profile["log_policy"],
        "max_file_bytes",
        "max_file_bytes",
        "B",
    )?;
    // SPEC §7 / T14, ADR 0012: a defaulted deep-park value is snapshotted with
    // its `default` provenance. The rebuilt host omits the switch so resolution
    // re-derives both; the exact-equality check below then rejects a snapshot
    // whose claimed value is not the default. Any other provenance value stays in
    // place and is refused as an unknown field.
    if profile["security"]["deep_park_source"] == "default" {
        let security = profile["security"]
            .as_object_mut()
            .ok_or_else(|| invalid("snapshot.profile.security", "security required"))?;
        security.remove("deep_park_source");
        security.remove("deep_park");
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
    // Encoded only when it differs from the default (QueuePolicy).
    if !queue["stream_idle_ms"].is_null() {
        unit(&mut queue, "stream_idle_ms", "stream_idle_timeout", "ms")?;
    }
    let host = json!({
        "schema_version": 1, "kind":"host", "name":h["name"],
        "hardware_fingerprint":h["hardware_fingerprint"], "environment_fingerprint":h["environment_fingerprint"],
        // Absent in the snapshot encodes as null and re-normalizes to None;
        // present, the host's published inventory digest round-trips so the
        // native launch can assert placement against it.
        "device_inventory_digest":h["device_inventory_digest"],
        "model_store": {"path": h["model_store"]},
        "runtime_profiles":{"snapshot":profile},
        "resource_policy": {
            "domains":domains, "devices":h["devices"], "max_parked":h["max_parked"],
            "observation_ttl":quantity(&h["observation_ttl_ms"], "ms")?, "device_sharing":h["device_sharing"],
            "endpoint_port_range":h["endpoint_port_range"], "planner_max_states":h["planner_max_states"], "queue":queue,
        },
    });
    Ok((deployment, host))
}

/// Rebuild the declared `engine_config` block from its resolved form (ADR 0014).
/// Values the provenance names as `mllm default` or `derived` are omitted so
/// resolution re-derives them; the exact-equality check then rejects a snapshot
/// whose claimed value differs. Returns the block, whether `resources` were
/// derived, and the checkpoint facts the derivation used.
pub(super) fn declared_engine_config(
    resolved: &Value,
) -> Result<(Value, bool, CheckpointFacts), ConfigError> {
    let object = resolved
        .as_object()
        .ok_or_else(|| invalid("snapshot.engine_config", "engine configuration required"))?;
    let provenance = object
        .get("provenance")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("snapshot.engine_config", "provenance required"))?;
    let declared = |field: &str| !provenance.contains_key(field);
    let engine = object.get("engine").and_then(Value::as_str);
    let mut block = serde_json::Map::new();
    let common = object
        .get("common")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("snapshot.engine_config", "common settings required"))?;
    for (field, value) in common {
        if declared(field) {
            block.insert(field.clone(), value.clone());
        }
    }
    let memory = &object["memory"];
    let mut raw_memory = serde_json::Map::new();
    for (field, key) in [
        ("memory.request", "request_bytes"),
        ("memory.kv_cache", "kv_cache_bytes"),
        ("memory.startup", "startup_bytes"),
    ] {
        // Owner decision 2026-09-23: a revision without a startup peak (one
        // resolved before the budget, or one declaring its resources) has
        // nothing to restate.
        if declared(field) && memory.get(key).is_some() {
            let name = field.trim_start_matches("memory.");
            raw_memory.insert(name.into(), json!(quantity(&memory[key], "B")?));
        }
    }
    if !raw_memory.is_empty() {
        block.insert("memory".into(), Value::Object(raw_memory));
    }
    let family: &[&str] = match engine {
        Some("vllm") => &["block_size_tokens", "max_num_batched_tokens"],
        Some("sglang") => &["max_total_tokens", "chunked_prefill_size", "tokenizer_workers"],
        _ => return Err(invalid("snapshot.engine_config", "unsupported engine")),
    };
    let prefix = engine.expect("engine checked above");
    let mut raw_family = serde_json::Map::new();
    for field in family {
        if let Some(value) = object.get(*field) {
            if declared(&format!("{prefix}.{field}")) {
                raw_family.insert((*field).into(), value.clone());
            }
        }
    }
    if !raw_family.is_empty() {
        block.insert(prefix.into(), Value::Object(raw_family));
    }
    let extra_args = object
        .get("extra_args")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("snapshot.engine_config", "extra_args required"))?;
    if !extra_args.is_empty() {
        block.insert("accept_extra_args".into(), json!(true));
        block.insert("extra_args".into(), Value::Array(extra_args.clone()));
    }
    let weights_bytes = match memory.get("weights_bytes") {
        None => None,
        Some(value) => Some(
            value
                .as_i64()
                .filter(|bytes| *bytes >= 0)
                .ok_or_else(|| invalid("snapshot.engine_config", "weights_bytes invalid"))?,
        ),
    };
    Ok((
        Value::Object(block),
        provenance.contains_key("resources"),
        CheckpointFacts {
            weights_bytes,
            // Owner decision 2026-09-23: a snapshot frozen before the startup
            // budget re-resolves with its cold phase equal to the request.
            legacy_startup: memory.get("startup_bytes").is_none(),
        },
    ))
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
