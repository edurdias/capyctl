//! SPEC §15.3 (2026-10-01 follow-up): the `resources` checks that need no host.
use super::{core, decode, raw_recipe, DeviceClaim, RawRecipe, TENSORFOLD_NEEDS_RESOURCES};
use crate::{ConfigError, ConfigErrorCode};
use serde_json::Value;

/// Decode a declared `resources` block with the types resolution uses and run
/// the intrinsic recipe and device-claim checks against the declared
/// `devices`. A TensorFold deployment must declare one (ADR 0023 §4); offline
/// the family is known from `runtime_profile: tensorfold` or an
/// `engine_config.tensorfold` block.
pub fn validate_declared_resources(deployment: &Value) -> Result<(), ConfigError> {
    let Some(resources) = deployment.get("resources") else {
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
    let recipe = raw_recipe(decode::<RawRecipe>(resources, "resources")?)?;
    let devices: Vec<DeviceClaim> = match deployment.get("devices") {
        Some(value) => decode(value, "devices")?,
        None => Vec::new(),
    };
    core::validate_resources_intrinsic(&recipe, &devices)
}
