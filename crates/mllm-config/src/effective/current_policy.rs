//! Pure composition of persisted resource controls into trusted local configuration.
use super::{compose_resource_policy, core, decode, invalid, HostInput};
use crate::resource_controls::{ResourceContext, ResourceControls};
use crate::ConfigError;
use serde_json::{json, Value};

/// Canonical command identity without mutable host/profile resolution. A replay
/// supplies its original resolved deadline, not the current host default. This
/// validates structure and intrinsic recipe rules only; acceptance must still
/// resolve the current profile and enforce current policy independently.
pub fn deployment_command_fingerprint(
    deployment: &Value,
    original_deadline_ms: i64,
) -> Result<String, ConfigError> {
    use super::{parse_duration_ms, phase, DeploymentInput, RecipeFootprints};
    use sha2::{Digest, Sha256};
    let mut input: DeploymentInput = decode(deployment, "deployment")?;
    if input.schema_version != 1
        || input.kind != "deployment"
        || input.name.is_empty()
        || input.runtime_profile.is_empty()
        || input.runtime_profile_revision == 0
        || input.routes.is_empty()
        || input.routes.iter().any(String::is_empty)
    {
        return Err(invalid("deployment", "invalid deployment command"));
    }
    input.routes.sort();
    if input.routes.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(invalid("routes", "duplicate route"));
    }
    let recipe = core::NormalizedRecipe {
        model: input.model,
        recipe: input.recipe,
        residency: input.residency,
        recovery: input.recovery,
        devices: input.devices,
        resources: RecipeFootprints {
            cold: phase(input.resources.cold)?,
            ready: phase(input.resources.ready)?,
            parking: phase(input.resources.parking)?,
            parked: phase(input.resources.parked)?,
            wake: phase(input.resources.wake)?,
        },
        request_deadline_ms: input
            .request_deadline
            .as_deref()
            .map(parse_duration_ms)
            .transpose()?
            .unwrap_or(original_deadline_ms),
    };
    core::validate_recipe_intrinsic(&recipe)?;
    let semantic = json!({
        "version": 1, "name": input.name, "routes": input.routes,
        "runtime_profile": input.runtime_profile, "runtime_profile_revision": input.runtime_profile_revision,
        "model": recipe.model, "recipe": recipe.recipe, "residency": recipe.residency,
        "recovery": recipe.recovery, "devices": recipe.devices, "resources": recipe.resources,
        "request_deadline_ms": recipe.request_deadline_ms,
    });
    let bytes = serde_json::to_vec(&semantic)
        .map_err(|_| invalid("deployment", "invalid deployment command"))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

/// Preserve immutable host/device/port context and every non-resource field while
/// replacing resource controls with a validated persisted snapshot. The caller
/// must obtain that snapshot from the owned Store, then revalidate its revision
/// in the accepting transaction. This pure helper grants no authority, imports no
/// policy, and does not validate a selected deployment or qualification record.
pub fn compose_current_resource_controls(
    trusted_host: &Value,
    context: &ResourceContext,
    controls: &ResourceControls,
) -> Result<Value, ConfigError> {
    let raw: HostInput = decode(trusted_host, "host")?;
    let normalized = core::normalize_host(raw)?;
    if ResourceContext::from_host(&normalized) != *context {
        return Err(invalid(
            "resource_policy",
            "immutable resource context mismatch",
        ));
    }
    controls.validate(context)?;
    // This must go through the one writer of the `resource_policy` shape,
    // `compose_resource_policy`, rather than build its own JSON: a second
    // hand-written composer of the same shape previously existed here, drifted
    // from the typed one, and defeated the compile-time guarantee that adding a
    // required field to `RawDomain` etc. cannot be silently omitted.
    let mut composed = trusted_host.clone();
    composed["resource_policy"] = compose_resource_policy(controls, context);
    Ok(composed)
}
