//! Carrying state written before ADR 0014 (E1) forward.
//!
//! Owner decision (2026-09-22): an upgrade migrates pre-E1 state instead of
//! refusing it. Before E1 an effective revision carried the engine tuning in the
//! host profile's `launch_settings`; after it, the deployment's `engine_config`
//! carries it (ADR 0014 §1). This module maps the retired shape onto the
//! deployment block that reproduces the same launch, and resolves it through the
//! same path a snapshot decode takes, so a migrated revision is exactly what
//! WE1's resolution produces for that block. A setting the new model cannot
//! express is refused with a diagnostic; it is never approximated.
//!
//! The recipe fingerprint of a migrated revision changes, because the
//! fingerprinted structure changed. The caller records the legacy fingerprint so
//! ownership and adoption keep using the identity the running engine was
//! launched under (SPEC §13.2: a running engine must not become foreign).

use super::*;
use serde_json::{json, Map, Value};

/// ADR 0014 §9: the one SGLang recipe mllm accepted before E1.
const PINNED_SGLANG_RECIPE: &str = "qwen3_4b_instruct2507_tp1_dp1_bf16_disk_reload_v1";

/// Why a pre-E1 record cannot be carried forward. The text names the setting and
/// what the operator can do; it never contains a credential.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct LegacyRefusal(pub String);

fn refuse(detail: impl Into<String>) -> LegacyRefusal {
    LegacyRefusal(detail.into())
}

/// A pre-E1 effective revision rewritten into the E1 shape.
#[derive(Debug, Clone)]
pub struct LegacyEffectiveMigration {
    /// The revision as WE1 resolution produces it for the mapped block.
    pub effective: EffectiveDeployment,
    /// Its canonical stored encoding.
    pub effective_json: String,
    /// The recipe fingerprint the revision carried before the upgrade.
    pub legacy_fingerprint: String,
    /// The deployment-side `engine_config` block the retired settings map to.
    pub engine_config: Value,
}

fn bytes(value: &Value, path: &str) -> Result<i64, LegacyRefusal> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .filter(|bytes| *bytes >= 0)
            .ok_or_else(|| refuse(format!("{path} is not a byte count"))),
        Value::String(text) => {
            parse_bytes(text).map_err(|_| refuse(format!("{path} is not a byte quantity")))
        }
        _ => Err(refuse(format!("{path} is missing"))),
    }
}

fn count(settings: &Value, field: &str) -> Result<u64, LegacyRefusal> {
    settings[field]
        .as_u64()
        .ok_or_else(|| refuse(format!("launch_settings.{field} is missing or not a count")))
}

fn flag(settings: &Value, field: &str) -> Result<Option<bool>, LegacyRefusal> {
    match &settings[field] {
        Value::Null => Ok(None),
        Value::Bool(value) => Ok(Some(*value)),
        _ => Err(refuse(format!("launch_settings.{field} is not a boolean"))),
    }
}

/// Map retired profile `launch_settings` (normalized or as a host document wrote
/// them) to the deployment `engine_config` block that renders the same launch.
///
/// `residency` and `deep_park_enabled` are the revision's own: sleep mode, the
/// SGLang memory saver and CPU weight backup are derived from them after E1, so a
/// retired value that disagrees with the derivation has no mapping.
pub fn legacy_engine_config(
    settings: &Value,
    engine: Engine,
    residency: Residency,
    deep_park_enabled: bool,
) -> Result<Value, LegacyRefusal> {
    let declared_engine = settings["engine"].as_str();
    let expected = match engine {
        Engine::Vllm => "vllm",
        Engine::Sglang => "sglang",
    };
    if declared_engine != Some(expected) {
        return Err(refuse(
            "launch_settings.engine does not match the runtime profile's engine",
        ));
    }
    let budget = &settings["requested_budget"];
    let kv_cache = bytes(
        &budget["kv_cache_bytes"],
        "launch_settings.requested_budget.kv_cache_bytes",
    )?;
    if kv_cache <= 0 {
        return Err(refuse(
            "launch_settings.requested_budget.kv_cache_bytes is not positive",
        ));
    }
    let mut block = Map::new();
    match engine {
        Engine::Vllm => {
            for field in ["tensor_parallel_size", "pipeline_parallel_size"] {
                let size = count(settings, field)?;
                if size != 1 {
                    return Err(refuse(format!(
                        "launch_settings.{field} is {size}: parallelism is reserved and \
                         rendered from placement by mllm, and a multi-rank vLLM \
                         launch has no mapping; stop the deployment and redeclare it"
                    )));
                }
            }
            if bytes(
                &settings["cpu_offload_bytes"],
                "launch_settings.cpu_offload_bytes",
            )? != 0
            {
                return Err(refuse(
                    "launch_settings.cpu_offload_bytes is not zero: CPU offload is reserved \
                     and not offered to deployments",
                ));
            }
            if bytes(
                &budget["swap_space_bytes"],
                "launch_settings.requested_budget.swap_space_bytes",
            )? != 0
            {
                return Err(refuse(
                    "launch_settings.requested_budget.swap_space_bytes is not zero: swap \
                     space is reserved and not offered to deployments",
                ));
            }
            let sleep = flag(settings, "enable_sleep_mode")?
                .ok_or_else(|| refuse("launch_settings.enable_sleep_mode is missing"))?;
            // SPEC §9.1 / ADR 0012: sleep mode is now derived. A parking
            // deployment on a host that parks would be parked through sleep
            // calls its running engine was never started to accept.
            if residency.parks() && deep_park_enabled && !sleep {
                return Err(refuse(
                    "launch_settings.enable_sleep_mode is false on a parking deployment: \
                     sleep mode is derived from residency and deep parking, \
                     and the running engine cannot be parked; stop it and redeclare the \
                     deployment with residency: restart_only",
                ));
            }
            let dtype = settings["kv_cache_dtype"]
                .as_str()
                .ok_or_else(|| refuse("launch_settings.kv_cache_dtype is missing"))?;
            block.insert("kv_cache_dtype".into(), json!(dtype));
            let block_size = count(settings, "block_size_tokens")?;
            block.insert("vllm".into(), json!({"block_size_tokens": block_size}));
        }
        Engine::Sglang => {
            if settings["recipe"].as_str() != Some(PINNED_SGLANG_RECIPE) {
                return Err(refuse(
                    "launch_settings.recipe is not the pinned SGLang recipe earlier \
                     releases accepted",
                ));
            }
            for field in ["tensor_parallel_size", "data_parallel_size"] {
                if settings.get(field).is_some() && count(settings, field)? != 1 {
                    return Err(refuse(format!(
                        "launch_settings.{field} is not 1: parallelism is reserved \
                         and rendered by mllm"
                    )));
                }
            }
            for field in [
                "speculative_decoding",
                "lora",
                "disaggregation",
                "external_cache",
                "cpu_kv_offload",
                "native_grpc",
            ] {
                if flag(settings, field)? == Some(true) {
                    return Err(refuse(format!(
                        "launch_settings.{field} is enabled and has no engine_config mapping"
                    )));
                }
            }
            let memory_saver = residency.parks();
            let cpu_weight_backup = residency == Residency::HostBacked;
            let weight_restore = if cpu_weight_backup {
                "cpu_backup"
            } else {
                "disk_reload"
            };
            if flag(settings, "memory_saver")?.is_some_and(|v| v != memory_saver)
                || flag(settings, "cpu_weight_backup")?.is_some_and(|v| v != cpu_weight_backup)
                || settings["weight_restore"]
                    .as_str()
                    .is_some_and(|v| v != weight_restore)
            {
                return Err(refuse(
                    "launch_settings disagree with the park strategy derived from the \
                     deployment's residency",
                ));
            }
            let prefill = flag(settings, "prefill_cuda_graphs")?.unwrap_or(false);
            let decode = flag(settings, "decode_cuda_graphs")?.unwrap_or(false);
            if prefill != decode {
                return Err(refuse(
                    "launch_settings enable CUDA graphs for only one of prefill and decode; \
                     engine_config.cuda_graphs covers both",
                ));
            }
            if flag(settings, "trust_remote_code")? == Some(true) {
                block.insert("trust_remote_code".into(), json!(true));
            }
            // The pinned recipe's values, carried explicitly so the launch does
            // not depend on later mllm defaults.
            let text =
                |field: &str, pinned: &str| settings[field].as_str().unwrap_or(pinned).to_owned();
            let number = |field: &str, pinned: u64| settings[field].as_u64().unwrap_or(pinned);
            block.insert("dtype".into(), json!(text("model_dtype", "bfloat16")));
            block.insert(
                "context_length".into(),
                json!(number("context_tokens", 4096)),
            );
            block.insert(
                "max_concurrent_requests".into(),
                json!(number("max_running_requests", 8)),
            );
            block.insert("cuda_graphs".into(), json!(prefill));
            block.insert(
                "sglang".into(),
                json!({
                    "max_total_tokens": number("max_total_tokens", 4096),
                    "tokenizer_workers": number("tokenizer_workers", 1),
                }),
            );
        }
    }
    // ADR 0014 §5: the requested KV becomes the declared KV cache; the memory
    // request is derived from the declared Ready total, as admission reserved it.
    block.insert("memory".into(), json!({"kv_cache": format!("{kv_cache}B")}));
    Ok(Value::Object(block))
}

/// Whether a stored effective revision predates E1.
pub fn is_legacy_effective(value: &Value) -> bool {
    value["profile"].get("launch_settings").is_some() && value.get("engine_config").is_none()
}

/// Rewrite one pre-E1 effective revision into the E1 shape, or `Ok(None)` when
/// it already has that shape.
pub fn migrate_legacy_effective(
    text: &str,
) -> Result<Option<LegacyEffectiveMigration>, LegacyRefusal> {
    let value: Value = serde_json::from_str(text)
        .map_err(|_| refuse("the stored effective revision is not JSON"))?;
    if !is_legacy_effective(&value) {
        return Ok(None);
    }
    let legacy_fingerprint = value["recipe_fingerprint"]
        .as_str()
        .filter(|fingerprint| !fingerprint.is_empty())
        .ok_or_else(|| refuse("the stored effective revision has no recipe fingerprint"))?
        .to_owned();
    let engine: Engine = serde_json::from_value(value["profile"]["engine"].clone())
        .map_err(|_| refuse("the stored runtime profile names no supported engine"))?;
    let residency: Residency = serde_json::from_value(value["residency"].clone())
        .map_err(|_| refuse("the stored revision names no residency"))?;
    let deep_park_enabled = value["profile"]["security"]["deep_park"] != "disabled";
    let engine_config = legacy_engine_config(
        &value["profile"]["launch_settings"],
        engine,
        residency,
        deep_park_enabled,
    )?;
    let mut rebuilt = value.clone();
    rebuilt["profile"]
        .as_object_mut()
        .ok_or_else(|| refuse("the stored runtime profile is not an object"))?
        .remove("launch_settings");
    let (deployment, host) = snapshot::snapshot_inputs(&rebuilt, engine_config.clone(), false)
        .map_err(|error| refuse(format!("the stored revision cannot be rebuilt: {error}")))?;
    let effective = resolve_effective(&deployment, &host).map_err(|error| {
        refuse(format!(
            "the mapped engine configuration does not resolve: {error}"
        ))
    })?;
    let effective_json = serde_json::to_string(&effective)
        .map_err(|_| refuse("the migrated revision could not be encoded"))?;
    // Everything outside the engine configuration must be carried unchanged; a
    // difference means the record holds something this mapping does not know.
    let migrated: Value = serde_json::from_str(&effective_json)
        .map_err(|_| refuse("the migrated revision could not be encoded"))?;
    let comparable = |mut value: Value| {
        if let Some(object) = value.as_object_mut() {
            object.remove("engine_config");
            object.remove("recipe_fingerprint");
            // ADR 0014 amendment A1: derived on migration; a pre-E1 revision
            // declared none.
            object.remove("timeouts");
        }
        if let Some(security) = value["profile"]["security"].as_object_mut() {
            for field in ["extra_args", "approved_options", "approved_paths"] {
                security.remove(field);
            }
        }
        value
    };
    let (before, after) = (comparable(rebuilt), comparable(migrated));
    if before != after {
        let field = before
            .as_object()
            .and_then(|object| {
                object
                    .keys()
                    .find(|key| before[key.as_str()] != after[key.as_str()])
                    .cloned()
            })
            .unwrap_or_else(|| "a field".into());
        return Err(refuse(format!(
            "the stored revision's `{field}` changes under migration; it holds a shape \
             this upgrade does not know"
        )));
    }
    // The decode every later reader performs must accept the result.
    decode_effective_snapshot(&effective_json)
        .map_err(|error| refuse(format!("the migrated revision does not decode: {error}")))?;
    Ok(Some(LegacyEffectiveMigration {
        effective,
        effective_json,
        legacy_fingerprint,
        engine_config,
    }))
}

/// Remove retired `runtime_profiles.*.launch_settings` from a stored host
/// document. Returns the stripped document and what was removed, or `None` when
/// the document carries none. A host's own YAML is configuration and stays
/// refused at parse with a pointer (ADR 0014 §1); this is for stored copies.
pub fn strip_legacy_launch_settings(document: &Value) -> Option<(Value, Value)> {
    let mut stripped = document.clone();
    let profiles = stripped.get_mut("runtime_profiles")?.as_object_mut()?;
    let mut removed = Map::new();
    for (name, profile) in profiles.iter_mut() {
        if let Some(settings) = profile
            .as_object_mut()
            .and_then(|p| p.remove("launch_settings"))
        {
            removed.insert(name.clone(), settings);
        }
    }
    (!removed.is_empty()).then_some((stripped, Value::Object(removed)))
}

/// The deployment a host agent resolves for a launch it journaled before E1 and
/// still owns, or `None` when the command already has the E1 shape.
///
/// The pre-E1 command names no `engine_config` and its tuning lived in host
/// settings the operator has since removed, so the original values are not on
/// the host. The KV cache is declared as the Ready total admission reserved: an
/// upper bound, used only to question, park or restore the engine that is
/// already running. It must never render a launch.
pub fn legacy_retained_deployment(config: &Value) -> Option<Value> {
    if config.get("engine_config").is_some() {
        return None;
    }
    let ready = config["resources"]["ready"]["allocations"].as_array()?;
    let mut total: i64 = 0;
    for allocation in ready {
        total = total.checked_add(parse_bytes(allocation["bytes"].as_str()?).ok()?)?;
    }
    if total <= 0 {
        return None;
    }
    let mut migrated = config.clone();
    migrated.as_object_mut()?.insert(
        "engine_config".into(),
        json!({"memory": {"kv_cache": format!("{total}B")}}),
    );
    Some(migrated)
}
