//! Pure composition of persisted resource controls into trusted local configuration.
use super::{core, decode, invalid, HostInput};
use crate::resource_controls::{ResourceContext, ResourceControls};
use crate::ConfigError;
use serde_json::{json, Map, Value};

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
    let domains: Map<String, Value> = controls
        .domains
        .iter()
        .map(|(id, domain)| {
            let mut value = json!({
                "managed_limit": format!("{}B", domain.managed_limit),
                "free_reserve": format!("{}B", domain.free_reserve),
                // SPEC §6.2: a domain's memory topology is a declared hardware fact,
                // not a runtime control, but it is required on every domain, so it
                // must round-trip through composition like the other required fields.
                "memory": domain.memory,
            });
            if let Some(bytes) = domain.host_kv_limit {
                value["host_kv_limit"] = json!(format!("{bytes}B"));
            }
            if let Some(bytes) = domain.parked_limit {
                value["parked_limit"] = json!(format!("{bytes}B"));
            }
            (id.clone(), value)
        })
        .collect();
    let devices: Map<String, Value> = context
        .device_domains
        .iter()
        .map(|(id, domain)| {
            (
                id.clone(),
                json!({"domain": domain, "sharing": controls.device_sharing_overrides[id]}),
            )
        })
        .collect();
    let mut composed = trusted_host.clone();
    composed["resource_policy"] = json!({
        "domains": domains,
        "devices": devices,
        "max_parked": controls.max_parked,
        "observation_ttl": format!("{}ms", controls.observation_ttl_ms),
        "device_sharing": controls.device_sharing,
        "endpoint_port_range": context.endpoint_port_range,
        "planner_max_states": controls.planner_max_states,
        "queue": {
            "max_pending_per_deployment": controls.queue.max_pending_per_deployment,
            "max_pending_total": controls.queue.max_pending_total,
            "max_buffered_bytes_total": format!("{}B", controls.queue.max_buffered_bytes_total),
            "request_deadline": format!("{}ms", controls.queue.request_deadline_ms),
            "admission_window": format!("{}ms", controls.queue.admission_window_ms),
        },
    });
    Ok(composed)
}
