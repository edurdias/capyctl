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
    use super::{engine_config, parse_duration_ms, raw_recipe, DeploymentInput, RecipeFootprints};
    use sha2::{Digest, Sha256};
    // ADR 0013 §2: a claim stated by sharing mode alone names no device; its
    // identity is its position, the same on every host.
    // The short form of `resources` is the command's own statement: its
    // figures are its identity, and the phases it stands for need a host.
    let short = crate::short_resources::check(deployment)?;
    let mut source = crate::instances::identity_devices(deployment);
    if short.is_some() {
        if let Some(object) = source.as_object_mut() {
            object.remove("resources");
        }
    }
    let mut input: DeploymentInput = decode(&source, "deployment")?;
    if input.schema_version != 1
        || input.kind != "deployment"
        || input.name.is_empty()
        || input.runtime_profile.is_empty()
        || input.runtime_profile_revision == Some(0)
        || input.routes.is_empty()
        || input.routes.iter().any(String::is_empty)
    {
        return Err(invalid("deployment", "invalid deployment command"));
    }
    input.routes.sort();
    if input.routes.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(invalid("routes", "duplicate route"));
    }
    // No host is in hand here, so nothing is resolved against a model store:
    // a command's identity must depend on the command alone.
    let model = core::normalize_model(input.model, None, None)?;
    let request_deadline_ms = input
        .request_deadline
        .as_deref()
        .map(parse_duration_ms)
        .transpose()?
        .unwrap_or(original_deadline_ms);
    let devices = input.devices.clone().unwrap_or_default();
    core::validate_identity_intrinsic(&model, &input.recipe, &devices, request_deadline_ms)?;
    // ADR 0014 §5: resources may be omitted and derived at resolution; the
    // command identity then carries none.
    let resources: Option<RecipeFootprints> = input.resources.map(raw_recipe).transpose()?;
    if let Some(resources) = &resources {
        core::validate_resources_intrinsic(resources, &devices)?;
    }
    // ADR 0014 §1: the deployment's engine configuration is part of what the
    // command asks for, so two commands differing only there are different.
    let engine_config = engine_config::declared_engine_config(&input.engine_config)?;
    let instance_spec = crate::instances::parse_instance_spec(deployment)?;
    let mut semantic = json!({
        "version": 2, "name": input.name, "routes": input.routes,
        "runtime_profile": input.runtime_profile, "runtime_profile_revision": input.runtime_profile_revision,
        "model": model, "recipe": input.recipe, "residency": input.residency,
        "recovery": input.recovery, "devices": input.devices, "resources": match short {
            Some([(_, gpu), (_, ram)]) => json!({"gpu": gpu, "ram": ram}),
            None => json!(resources),
        },
        "request_deadline_ms": request_deadline_ms, "engine_config": engine_config,
    });
    // ADR 0013 §7: the instance count and placement are part of what the
    // command asks for. The default adds nothing, so a command written before
    // instances existed keeps its fingerprint across the upgrade.
    if let Some(identity) = instance_spec.command_identity() {
        semantic["instances"] = identity;
    }
    // ADR 0014 amendment A1: declared timeouts are part of what the command
    // asks for. Omitted, they add nothing, so older commands keep their
    // fingerprint.
    if let Some(block) = deployment.get("timeouts") {
        super::validate_declared_timeouts(deployment)?;
        // Normalized, so `600s` and `10m` are the same command.
        let mut normalized = serde_json::Map::new();
        for field in ["initialize", "wake"] {
            if let Some(text) = block.get(field).and_then(Value::as_str) {
                normalized.insert(format!("{field}_ms"), json!(parse_duration_ms(text)?));
            }
        }
        semantic["timeouts"] = Value::Object(normalized);
    }
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
    let raw: HostInput = super::decode_host(trusted_host)?;
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
    // SPEC §3 / T16: physical placement is trusted host context, not a mutable
    // resource control. Preserve it when composing the current limits.
    for (id, device) in &normalized.devices {
        if let Some(uuid) = &device.physical_gpu_uuid {
            composed["resource_policy"]["devices"][id]["physical_gpu_uuid"] = json!(uuid);
        }
    }
    Ok(composed)
}
