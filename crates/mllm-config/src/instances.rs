//! ADR 0013 §1–3: a deployment's instance count and placement constraints.
//!
//! A deployment declares `instances: N` (default 1) and optional `placement`
//! constraints; the scheduler places each instance when it activates (ADR 0013
//! §4, unit I2). This module only parses and validates the declaration. It never
//! chooses a host, reserves capacity or reads live state.
//!
//! The `host: <name>` field is shorthand for `placement.hosts: [<name>]`, so a
//! deployment written before instances existed keeps its meaning with one
//! instance pinned to that host.

use crate::{ConfigError, ConfigErrorCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Upper bound on a deployment's instance count. Every instance holds its own
/// reservation, binding and residual floor (ADR 0013 open issue 6), so the count
/// is bounded like every other durable collection.
pub const MAX_INSTANCES: u32 = 64;

/// ADR 0013 §4 step 3: how the scheduler orders candidate hosts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementStrategy {
    /// Prefer the host with fewest instances of this deployment (default).
    #[default]
    Spread,
    /// Prefer the host with most instances of this deployment, best fit.
    Pack,
}

/// ADR 0013 §2: the deployment's placement constraints, normalized.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Placement {
    /// Allowed host set; `None` means every enrolled host.
    pub hosts: Option<Vec<String>>,
    /// Host-label match, ANDed with `hosts`. Labels are host policy, never
    /// asserted by the deployment.
    #[serde(default)]
    pub selector: BTreeMap<String, String>,
    #[serde(default)]
    pub strategy: PlacementStrategy,
    /// `None` is unbounded.
    pub max_per_host: Option<u32>,
}

impl Placement {
    /// Whether `host` is in the allowed host set (every host when unset).
    pub fn allows(&self, host: &str) -> bool {
        self.hosts
            .as_ref()
            .is_none_or(|hosts| hosts.iter().any(|allowed| allowed == host))
    }

    /// ADR 0013 §2: whether a host's labels satisfy the selector (every
    /// selected label present with the same value). An empty selector matches
    /// every host.
    pub fn selector_matches(&self, labels: &BTreeMap<String, String>) -> bool {
        self.selector
            .iter()
            .all(|(label, value)| labels.get(label) == Some(value))
    }

    /// Whether the allowed set is exactly one named host (a pinned deployment).
    pub fn pinned_host(&self) -> Option<&str> {
        match self.hosts.as_deref() {
            Some([host]) => Some(host.as_str()),
            _ => None,
        }
    }
}

/// ADR 0013 §1–2: instance count plus placement constraints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstanceSpec {
    pub instances: u32,
    pub placement: Placement,
    /// SPEC §6.5 warm-residency commitment (ADR 0013 amendment 2026-09-23),
    /// declared as `lifecycle.warm: true`: the deployment's instances are
    /// never evicted by switching, never parked or stopped by the idle policy
    /// and never reclaimed from the parked set by capacity; only an explicit
    /// stop ends their residency. Absent unless declared, so a command
    /// written before it existed keeps its idempotency fingerprint.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub warm: bool,
}

impl Default for InstanceSpec {
    fn default() -> Self {
        Self {
            instances: 1,
            placement: Placement::default(),
            warm: false,
        }
    }
}

impl InstanceSpec {
    /// The part of a deployment command's identity this declaration adds, or
    /// `None` for the default (one instance, no constraints). A command written
    /// before instances existed therefore keeps its idempotency fingerprint.
    pub fn command_identity(&self) -> Option<Value> {
        (*self != Self::default()).then(|| serde_json::to_value(self).expect("plain data encodes"))
    }

    /// ADR 0013 §3: whether `resolving_hosts` hosts can hold `instances` under
    /// `max_per_host`. The deploy is refused as unplaceable otherwise.
    pub fn placeable_on(&self, resolving_hosts: usize) -> bool {
        match self.placement.max_per_host {
            None => resolving_hosts >= 1,
            Some(per_host) => {
                u64::from(per_host) * resolving_hosts as u64 >= u64::from(self.instances)
            }
        }
    }
}

/// ADR 0013 §2, §4 step 2: a deployment whose allowed set spans hosts states
/// its devices by sharing mode alone (`devices: [{sharing: shared}]`), because
/// a device id is local to one host. Resolving it against one host assigns
/// that host's devices, in id order, to the unnamed claims: an exclusive claim
/// takes any free device, a shared claim only one the host lets be shared.
/// Unnamed device claims in `resources` phases take the same devices in the
/// same order. Named claims are left as they are. A host without enough
/// suitable devices refuses the deployment.
pub fn assign_devices(deployment: &Value, host: &Value) -> Result<Value, ConfigError> {
    let unnamed = |claims: &Value| {
        claims
            .as_array()
            .map(|claims| claims.iter().filter(|c| c.get("id").is_none()).count())
            .unwrap_or(0)
    };
    if unnamed(&deployment["devices"]) == 0 {
        return Ok(deployment.clone());
    }
    let policy = &host["resource_policy"];
    let default_shared = policy["device_sharing"].as_str() == Some("shared");
    let mut free: Vec<(String, bool)> = policy["devices"]
        .as_object()
        .map(|devices| {
            devices
                .iter()
                .map(|(id, device)| {
                    let shared = match device["sharing"].as_str() {
                        Some(sharing) => sharing == "shared",
                        None => default_shared,
                    };
                    (id.clone(), shared)
                })
                .collect()
        })
        .unwrap_or_default();
    // ADR 0013 §2: a device a named claim already holds is not free for an
    // unnamed one; taking it again would double the claim.
    let named: Vec<&str> = deployment["devices"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|claim| claim.get("id").and_then(Value::as_str))
        .collect();
    free.retain(|(id, _)| !named.contains(&id.as_str()));
    free.sort();
    let mut assigned = Vec::new();
    let mut result = deployment.clone();
    for claim in result["devices"]
        .as_array_mut()
        .ok_or_else(|| invalid("devices", "must be a list"))?
    {
        if claim.get("id").is_some() {
            continue;
        }
        let shared = claim["sharing"].as_str() == Some("shared");
        let position = free
            .iter()
            .position(|(_, allows)| !shared || *allows)
            .ok_or_else(|| invalid("devices", "the host has no free device for the claim"))?;
        let (id, _) = free.remove(position);
        claim["id"] = Value::String(id.clone());
        assigned.push(id);
    }
    if let Some(phases) = result.get_mut("resources").and_then(Value::as_object_mut) {
        for phase in phases.values_mut() {
            let Some(claims) = phase.get_mut("devices").and_then(Value::as_array_mut) else {
                continue;
            };
            let mut next = assigned.iter();
            for claim in claims.iter_mut().filter(|c| c.get("id").is_none()) {
                let id = next.next().ok_or_else(|| {
                    invalid("resources.devices", "more unnamed claims than devices")
                })?;
                claim["id"] = Value::String(id.clone());
            }
        }
    }
    Ok(result)
}

/// Discrete GPU design §7 (ADR 0019, owner decision 3): on a host whose GPUs
/// are device-memory domains, a deployment that pins no device is resolved
/// once per GPU, and placement picks the GPU. Each choice is the deployment
/// with that device selected (`devices: [{id, sharing}]`), lowest driver
/// index first; the first is what the host's own resolution records.
///
/// A deployment offers a choice when it declares no device, or one unnamed
/// claim (the multi-host shape, whose sharing it keeps; a shared claim takes
/// only a device the host lets be shared). There is no choice (an empty list)
/// when the host has no device domain, when the deployment names a device
/// (`devices: [{id: gpuN}]` pins it), declares several claims (refused
/// `multi_gpu_unsupported` by resolution), or declares explicit `resources`,
/// which name their domains and so their device.
pub fn device_choices(
    deployment: &Value,
    host: &Value,
) -> Result<Vec<(String, Value)>, ConfigError> {
    let policy = &host["resource_policy"];
    let default_sharing = policy["device_sharing"].as_str().unwrap_or("exclusive");
    let mut devices: Vec<(String, String)> = policy["devices"]
        .as_object()
        .map(|devices| {
            devices
                .iter()
                .filter(|(_, device)| {
                    device["domain"]
                        .as_str()
                        .is_some_and(|domain| policy["domains"][domain]["memory"] == "device")
                })
                .map(|(id, device)| {
                    let sharing = device["sharing"].as_str().unwrap_or(default_sharing);
                    (id.clone(), sharing.to_owned())
                })
                .collect()
        })
        .unwrap_or_default();
    if devices.is_empty() || deployment.get("resources").is_some_and(|r| !r.is_null()) {
        return Ok(Vec::new());
    }
    let claims = match deployment.get("devices") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(claims)) => claims.clone(),
        Some(_) => return Err(invalid("devices", "must be a list")),
    };
    let wanted = match claims.as_slice() {
        [] => None,
        [claim] if claim.get("id").is_none() => claim["sharing"].as_str(),
        _ => return Ok(Vec::new()),
    };
    if wanted == Some("shared") {
        devices.retain(|(_, sharing)| sharing == "shared");
    }
    let index = |id: &str| -> u32 {
        id.strip_prefix("gpu")
            .and_then(|n| n.parse().ok())
            .unwrap_or(u32::MAX)
    };
    devices.sort_by(|(a, _), (b, _)| index(a).cmp(&index(b)).then_with(|| a.cmp(b)));
    Ok(devices
        .into_iter()
        .map(|(id, sharing)| {
            let mut choice = deployment.clone();
            choice["devices"] = serde_json::json!([{
                "id": id,
                "sharing": wanted.unwrap_or(&sharing),
            }]);
            (id, choice)
        })
        .collect())
}

/// The command identity of unnamed device claims: each takes the placeholder
/// `unnamed/<n>` by position, in `devices` and in every `resources` phase, so
/// a command's identity never depends on the host that resolves it. Named
/// claims are left as they are.
pub fn identity_devices(deployment: &Value) -> Value {
    let mut result = deployment.clone();
    let fill = |claims: &mut Value| {
        if let Some(claims) = claims.as_array_mut() {
            for (n, claim) in claims
                .iter_mut()
                .filter(|claim| claim.is_object() && claim.get("id").is_none())
                .enumerate()
            {
                claim["id"] = Value::String(format!("unnamed/{n}"));
            }
        }
    };
    if let Some(claims) = result.get_mut("devices") {
        fill(claims);
    }
    if let Some(phases) = result.get_mut("resources").and_then(Value::as_object_mut) {
        for phase in phases.values_mut() {
            if let Some(claims) = phase.get_mut("devices") {
                fill(claims);
            }
        }
    }
    result
}

/// The most labels one host publishes.
pub const MAX_LABELS: usize = 64;

/// ADR 0013 §2: validate a host's placement labels (`resource_policy.labels`):
/// names and values are bounded identifiers.
pub fn validate_labels(labels: &BTreeMap<String, String>) -> Result<(), ConfigError> {
    if labels.len() > MAX_LABELS
        || labels
            .iter()
            .any(|(label, value)| !identifier(label) || !identifier(value))
    {
        return Err(invalid(
            "resource_policy.labels",
            "labels are at most 64 name-value pairs of names",
        ));
    }
    Ok(())
}

/// ADR 0013 §2: the placement labels a host document publishes
/// (`resource_policy.labels`), empty when it states none.
pub fn host_labels(host: &Value) -> Result<BTreeMap<String, String>, ConfigError> {
    let Some(raw) = host["resource_policy"]
        .get("labels")
        .filter(|v| !v.is_null())
    else {
        return Ok(BTreeMap::new());
    };
    let labels: BTreeMap<String, String> = raw
        .as_object()
        .ok_or_else(|| invalid("resource_policy.labels", "must be a label mapping"))?
        .iter()
        .map(|(label, value)| value.as_str().map(|v| (label.clone(), v.to_owned())))
        .collect::<Option<_>>()
        .ok_or_else(|| invalid("resource_policy.labels", "label values are names"))?;
    validate_labels(&labels)?;
    Ok(labels)
}

fn invalid(path: &str, detail: &str) -> ConfigError {
    ConfigError::new(ConfigErrorCode::UnsupportedCombination, path, detail)
}

fn identifier(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

fn bounded_count(value: &Value, path: &str, max: u32) -> Result<u32, ConfigError> {
    value
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| (1..=max).contains(n))
        .ok_or_else(|| invalid(path, "must be an integer between 1 and the bound"))
}

/// Parse and validate a deployment document's `instances`, `placement` and
/// `host` shorthand (ADR 0013 §2). Fields absent mean one unconstrained
/// instance.
pub fn parse_instance_spec(deployment: &Value) -> Result<InstanceSpec, ConfigError> {
    let object = deployment
        .as_object()
        .ok_or_else(|| invalid("deployment", "must be a mapping"))?;
    let instances = match object.get("instances") {
        None | Some(Value::Null) => 1,
        Some(value) => bounded_count(value, "instances", MAX_INSTANCES)?,
    };
    let mut placement = Placement::default();
    if let Some(raw) = object.get("placement").filter(|v| !v.is_null()) {
        let raw = raw
            .as_object()
            .ok_or_else(|| invalid("placement", "must be a mapping"))?;
        for key in raw.keys() {
            if !matches!(
                key.as_str(),
                "hosts" | "selector" | "strategy" | "max_per_host"
            ) {
                return Err(ConfigError::new(
                    ConfigErrorCode::UnknownField,
                    format!("placement.{key}"),
                    "unknown placement field",
                ));
            }
        }
        if let Some(hosts) = raw.get("hosts") {
            let hosts = hosts
                .as_array()
                .ok_or_else(|| invalid("placement.hosts", "must be a list of host names"))?;
            let names: Vec<String> = hosts
                .iter()
                .map(|h| h.as_str().filter(|h| identifier(h)).map(str::to_owned))
                .collect::<Option<_>>()
                .ok_or_else(|| invalid("placement.hosts", "every entry must be a host name"))?;
            if names.is_empty() {
                return Err(invalid("placement.hosts", "must name at least one host"));
            }
            if names.iter().collect::<BTreeSet<_>>().len() != names.len() {
                return Err(invalid("placement.hosts", "must not repeat a host"));
            }
            placement.hosts = Some(names);
        }
        if let Some(selector) = raw.get("selector") {
            let selector = selector
                .as_object()
                .ok_or_else(|| invalid("placement.selector", "must be a label mapping"))?;
            for (label, value) in selector {
                let value = value
                    .as_str()
                    .filter(|v| identifier(v) && identifier(label))
                    .ok_or_else(|| invalid("placement.selector", "labels and values are names"))?;
                placement.selector.insert(label.clone(), value.to_owned());
            }
        }
        if let Some(strategy) = raw.get("strategy") {
            placement.strategy = match strategy.as_str() {
                Some("spread") => PlacementStrategy::Spread,
                Some("pack") => PlacementStrategy::Pack,
                _ => return Err(invalid("placement.strategy", "must be spread or pack")),
            };
        }
        if let Some(max) = raw.get("max_per_host") {
            placement.max_per_host =
                Some(bounded_count(max, "placement.max_per_host", MAX_INSTANCES)?);
        }
    }
    // ADR 0013 §2: `host` is shorthand for a one-host allowed set; stating both
    // would leave two answers to "where may this run".
    if let Some(host) = object.get("host").filter(|v| !v.is_null()) {
        let host = host
            .as_str()
            .filter(|h| identifier(h))
            .ok_or_else(|| invalid("host", "must be a host name"))?;
        if placement.hosts.is_some() {
            return Err(invalid(
                "host",
                "`host` is shorthand for `placement.hosts`; state one of them",
            ));
        }
        placement.hosts = Some(vec![host.to_owned()]);
    }
    // SPEC §6.5 (ADR 0013 amendment 2026-09-23): the warm commitment.
    let mut warm = false;
    if let Some(raw) = object.get("lifecycle").filter(|v| !v.is_null()) {
        let raw = raw
            .as_object()
            .ok_or_else(|| invalid("lifecycle", "must be a mapping"))?;
        for key in raw.keys() {
            if key != "warm" {
                return Err(ConfigError::new(
                    ConfigErrorCode::UnknownField,
                    format!("lifecycle.{key}"),
                    "unknown lifecycle field",
                ));
            }
        }
        if let Some(value) = raw.get("warm") {
            warm = value
                .as_bool()
                .ok_or_else(|| invalid("lifecycle.warm", "must be true or false"))?;
        }
    }
    let spec = InstanceSpec {
        instances,
        placement,
        warm,
    };
    // ADR 0013 §3: with an explicit allowed set the count must fit it.
    if let Some(hosts) = &spec.placement.hosts {
        if !spec.placeable_on(hosts.len()) {
            return Err(invalid(
                "instances",
                "unplaceable: max_per_host times the allowed hosts is below instances",
            ));
        }
    }
    // ADR 0013 §2: a device id is local to one host, so naming devices needs an
    // allowed set of exactly one host. A device count chosen by the scheduler
    // is the multi-host shape (placement unit I2).
    let names_devices = object
        .get("devices")
        .and_then(Value::as_array)
        .is_some_and(|devices| devices.iter().any(|d| d.get("id").is_some()));
    if names_devices && spec.placement.hosts.as_ref().is_some_and(|h| h.len() > 1) {
        return Err(invalid(
            "devices",
            "named devices require an allowed set of exactly one host",
        ));
    }
    Ok(spec)
}
