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
    let (engine_config, resources_derived, facts) =
        declared_engine_config(&value["engine_config"])?;
    let (deployment, host) = snapshot_inputs(&value, engine_config, resources_derived)?;
    let mut effective = resolve_effective_with_checkpoint(&deployment, &host, facts)?;
    // ADR 0014 amendment A7 (2026-10-02): a vLLM or SGLang revision frozen
    // before the first-start allowance keeps the derived Initialize window it
    // was frozen with, the load term alone; any other claimed value is refused.
    if let Some(frozen) = frozen_load_term_initialize(&effective, &value) {
        effective.timeouts.initialize_ms = frozen;
    }
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

/// The Initialize window a snapshot claims, when it is the one derived before
/// the first-start allowance existed: derived, for vLLM or SGLang, from known
/// weights, and equal to the load term lowered to the request deadline.
fn frozen_load_term_initialize(effective: &EffectiveDeployment, value: &Value) -> Option<i64> {
    let claimed = value["timeouts"]["initialize_ms"].as_i64()?;
    // The timeouts are derived from the whole checkpoint, a group member's too.
    let weights = effective
        .engine_config
        .memory()
        .checkpoint_weights_bytes()
        .filter(|bytes| *bytes > 0)?;
    let load_term = derived_initialize_ms(Some(weights)).min(effective.request_deadline_ms);
    (matches!(effective.profile.engine, Engine::Vllm | Engine::Sglang)
        && effective.timeouts.provenance.get("initialize") == Some(&TimeoutSource::Derived)
        && claimed == load_term
        && claimed != effective.timeouts.initialize_ms)
        .then_some(claimed)
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
    let object = model
        .as_object_mut()
        .ok_or_else(|| invalid("snapshot.model", "model required"))?;
    object.remove("resolved_path");
    // ADR 0008 amendment 2026-10-08: likewise the drafter's directory; the
    // deployment states its source.
    if let Some(draft) = object.get_mut("draft") {
        *draft = draft["source"].clone();
    }
    let mut deployment = json!({
        "schema_version": value["schema_version"], "kind": "deployment",
        "name": value["name"], "model": model, "routes": value["routes"],
        "runtime_profile": "snapshot", "runtime_profile_revision": value["profile"]["revision"],
        "recipe": value["recipe"], "residency": value["residency"], "recovery": value["recovery"],
        "devices": value["selected_devices"], "resources": resources,
        "request_deadline": quantity(&value["request_deadline_ms"], "ms")?,
        "engine_config": engine_config,
    });
    // ADR 0028 §2.1: the deployment's engine environment is stored with its
    // values in `engine_env`; its deployment entries are restated as declared,
    // and the exact-equality check below proves the claim.
    if let Some(vars) = value["engine_env"]["vars"].as_object() {
        let mut env = serde_json::Map::new();
        for (name, entry) in vars {
            if entry[1] == "deployment" {
                env.insert(name.clone(), entry[0].clone());
            }
        }
        if !env.is_empty() {
            deployment["engine_config"]
                .as_object_mut()
                .ok_or_else(|| invalid("snapshot.engine_config", "engine configuration required"))?
                .insert("env".into(), Value::Object(env));
        }
    }
    // Owner decision 2026-09-25: a residency capyctl chose is chosen again from
    // the same host and facts (or from newly measured weights on a
    // re-resolution), and the exact-equality check below proves the claim.
    if value["engine_config"]["provenance"]
        .get("residency")
        .is_some()
    {
        deployment
            .as_object_mut()
            .expect("deployment is an object")
            .remove("residency");
    }
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
            timeouts.insert(
                field.into(),
                json!(quantity(&value["timeouts"][key], "ms")?),
            );
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
    let mut host = json!({
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
    // ADR 0014 amendment A19: encoded only when stated, in its written form.
    if !h["parked_growth_limit"].is_null() {
        host["resource_policy"]["parked_growth_limit"] = h["parked_growth_limit"].clone();
    }
    // ADR 0008: encoded only when stated; restated in its written form.
    if !h["model_sources"].is_null() {
        let mut sources = h["model_sources"].clone();
        if !sources["max_bytes"].is_null() {
            unit(&mut sources, "max_bytes", "max_bytes", "B")?;
        }
        host["model_sources"] = sources;
    }
    Ok((deployment, host))
}

/// Rebuild the declared `engine_config` block from its resolved form (ADR 0014).
/// Values the provenance names as `capyctl default` or `derived` are omitted so
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
        Some("vllm") => &[
            "block_size_tokens",
            "max_num_batched_tokens",
            "safetensors_load_strategy",
            "tool_call_parser",
            "reasoning_parser",
        ],
        Some("sglang") => &[
            "max_total_tokens",
            "chunked_prefill_size",
            "tokenizer_workers",
            "tool_call_parser",
            "reasoning_parser",
        ],
        Some("tensorfold") => &["max_tokens", "thinking"],
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
    // ADR 0014 amendment A16: the state slot the host measured beside the
    // weights; a revision without it keeps its sizing.
    let state_slot_bytes = match memory.get("state_slot_bytes") {
        None => None,
        Some(value) => Some(
            value
                .as_i64()
                .filter(|bytes| *bytes > 0)
                .ok_or_else(|| invalid("snapshot.engine_config", "state_slot_bytes invalid"))?,
        ),
    };
    // ADR 0028 §5 (amendment of 2026-10-07): a group member's derivation
    // records the whole checkpoint, its layout and the topology the share in
    // `weights_bytes` was taken of; it is re-derived from them.
    let (weights_bytes, layout, member_of) = match memory.get("member") {
        None => (weights_bytes, None, None),
        Some(member) => member_facts(member)?,
    };
    // ADR 0014 amendment A20: an engine that keeps the checkpoint's tables on
    // disk records the whole checkpoint and its tables; `weights_bytes` is
    // what stays in memory, re-derived from them.
    let (weights_bytes, disk_tables) = match memory.get("disk_tables") {
        None => (weights_bytes, None),
        Some(recorded) => {
            let (whole, tables) = disk_table_facts(recorded)?;
            (
                member_of.map_or(Some(whole), |_| weights_bytes),
                Some(tables),
            )
        }
    };
    Ok((
        Value::Object(block),
        provenance.contains_key("resources"),
        CheckpointFacts {
            weights_bytes,
            state_slot_bytes,
            layout,
            member_of,
            disk_tables,
            // Owner decision 2026-09-23: a snapshot frozen before the startup
            // budget re-resolves with its cold phase equal to the request.
            legacy_startup: memory.get("startup_bytes").is_none(),
            // Re-review parity rule: a snapshot frozen before the engine's
            // CUDA context was charged re-derives without it.
            legacy_overhead: memory.get("overhead_bytes").is_none(),
            // ADR 0014 amendment A8: a snapshot frozen before the first-start
            // graph allowance re-derives its placeholder startup without it.
            legacy_startup_graphs: memory.get("startup_graphs_bytes").is_none(),
            // ADR 0019 §3 (2026-10-03) and ADR 0014 amendment A18
            // (2026-10-07): the family margin recorded beside a declared
            // request on a discrete GPU, or on unified memory, is the rule
            // before the device margin and the margin that grows with the
            // weights.
            legacy_family_margin: memory.get("margin_bytes").and_then(Value::as_i64)
                == Some(super::engine_config::VLLM_OVERHEAD_MARGIN_BYTES),
            // ADR 0014 amendment A13: the graphs-off default beside the
            // memory saver, as frozen before it was dropped.
            legacy_sglang_graphs_off: engine == Some("sglang")
                && provenance.get("cuda_graphs").and_then(Value::as_str) == Some("capyctl default"),
        },
    ))
}

type MemberFacts = (
    Option<i64>,
    Option<capyctl_domain::member_weights::CheckpointLayout>,
    Option<crate::topology::Topology>,
);

/// The checkpoint facts and topology a snapshot's `memory.member` records.
fn member_facts(member: &Value) -> Result<MemberFacts, ConfigError> {
    let bad = || invalid("snapshot.engine_config", "member invalid");
    let count = |key: &str| {
        member
            .get(key)
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n >= 1)
            .ok_or_else(bad)
    };
    let bytes = |value: &Value, key: &str| value.get(key).and_then(Value::as_i64).ok_or_else(bad);
    let topology = crate::topology::Topology {
        tensor_parallel: count("tensor_parallel")?,
        pipeline_parallel: count("pipeline_parallel")?,
    };
    let weights = match member.get("checkpoint_weights_bytes") {
        None => None,
        Some(_) => Some(
            bytes(member, "checkpoint_weights_bytes")
                .and_then(|w| (w >= 0).then_some(w).ok_or_else(bad))?,
        ),
    };
    let layout = match member.get("layout") {
        None => None,
        Some(layout) => {
            let parsed = capyctl_domain::member_weights::CheckpointLayout {
                sharded_bytes: bytes(layout, "sharded_bytes")?,
                layer_count: layout
                    .get("layer_count")
                    .and_then(Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or_else(bad)?,
                largest_layer_bytes: bytes(layout, "largest_layer_bytes")?,
            };
            Some(parsed.is_valid().then_some(parsed).ok_or_else(bad)?)
        }
    };
    Ok((weights, layout, Some(topology)))
}

/// ADR 0014 amendment A20: the whole checkpoint's weights and the tables a
/// snapshot's `memory.disk_tables` records.
fn disk_table_facts(
    recorded: &Value,
) -> Result<(i64, capyctl_domain::disk_tables::CheckpointTables), ConfigError> {
    let bad = || invalid("snapshot.engine_config", "disk_tables invalid");
    let bytes = |value: &Value, key: &str| value.get(key).and_then(Value::as_i64).ok_or_else(bad);
    let whole = bytes(recorded, "checkpoint_weights_bytes")?;
    let tables = recorded.get("tables").ok_or_else(bad)?;
    let parsed = capyctl_domain::disk_tables::CheckpointTables {
        bytes: bytes(tables, "bytes")?,
        count: tables
            .get("count")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(bad)?,
        sharded_bytes: bytes(tables, "sharded_bytes")?,
        resident_largest_layer_bytes: bytes(tables, "resident_largest_layer_bytes")?,
    };
    if whole < 0 || !parsed.is_valid() {
        return Err(bad());
    }
    Ok((whole, parsed))
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
