//! SPEC §15.3 (2026-10-01 follow-up): the `resources` checks that need no host.
use super::{core, decode, raw_recipe, DeviceClaim, RawRecipe, TENSORFOLD_NEEDS_RESOURCES};
use crate::{ConfigError, ConfigErrorCode};
use serde_json::{json, Value};

/// Decode a declared `resources` block with the types resolution uses and run
/// the intrinsic recipe and device-claim checks against the declared
/// `devices`. A TensorFold deployment must declare one (ADR 0023 §4); offline
/// the family is known from `runtime_profile: tensorfold` or an
/// `engine_config.tensorfold` block.
pub fn validate_declared_resources(deployment: &Value) -> Result<(), ConfigError> {
    // `resources: null` is absent, as the server decodes it.
    let Some(resources) = deployment.get("resources").filter(|r| !r.is_null()) else {
        let tensorfold = ["runtime_profile", "engine"]
            .iter()
            .any(|key| deployment[*key].as_str() == Some("tensorfold"))
            || deployment["engine_config"].get("tensorfold").is_some();
        if tensorfold {
            return Err(ConfigError::new(
                ConfigErrorCode::MissingRequired,
                "resources",
                TENSORFOLD_NEEDS_RESOURCES,
            ));
        }
        return Ok(());
    };
    // Deploy fills a claim's missing `sharing` from the host (found live
    // 2026-10-01: the guide's block names devices without it). Offline the
    // host is unknown, so a missing one takes the deployment's own sharing for
    // that device, else `exclusive`, and only the shape is checked.
    let mut declared = deployment.get("devices").cloned();
    if let Some(Value::Array(claims)) = declared.as_mut() {
        fill_sharing(claims, &[]);
    }
    let stated: Vec<Value> = match &declared {
        Some(Value::Array(claims)) => claims.clone(),
        _ => Vec::new(),
    };
    let mut resources = resources.clone();
    if let Some(phases) = resources.as_object_mut() {
        for phase in phases.values_mut() {
            if let Some(Value::Array(claims)) = phase.get_mut("devices") {
                fill_sharing(claims, &stated);
            }
        }
    }
    let recipe = raw_recipe(decode::<RawRecipe>(&resources, "resources")?)?;
    let devices: Vec<DeviceClaim> = match &declared {
        Some(value) => decode(value, "devices")?,
        None => Vec::new(),
    };
    core::validate_resources_intrinsic(&recipe, &devices)
}

/// Give each named claim without `sharing` the sharing `stated` gives the same
/// device id, else `exclusive`.
fn fill_sharing(claims: &mut [Value], stated: &[Value]) {
    for claim in claims {
        let Some(object) = claim.as_object_mut() else {
            continue;
        };
        if object.contains_key("sharing") {
            continue;
        }
        let Some(id) = object.get("id").and_then(Value::as_str) else {
            continue;
        };
        let sharing = stated
            .iter()
            .find(|s| s["id"].as_str() == Some(id))
            .and_then(|s| s["sharing"].as_str())
            .unwrap_or("exclusive")
            .to_owned();
        object.insert("sharing".into(), json!(sharing));
    }
}
