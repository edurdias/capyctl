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

/// SPEC §6.5, §10: who may start, stop, park or wake a deployment, declared as
/// `lifecycle.activation` by the deployment and, for every deployment that
/// may run on it, by a host (its document's `lifecycle.activation`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activation {
    /// CapyCTL may also act on its own: an inference request starts or wakes
    /// the deployment, and the idle policy, request-driven switching and
    /// parked-capacity reclamation may park or stop it (default).
    #[default]
    OnDemand,
    /// Only an operator's action (start, stop, park, pre-initialize, delete,
    /// drain) changes it. A request for it while it is not serving is
    /// refused at once (`deployment_inactive`); it is never a switch victim,
    /// never idled and never reclaimed from the parked set.
    Explicit,
}

impl Activation {
    pub fn is_on_demand(&self) -> bool {
        *self == Self::OnDemand
    }

    /// The value stated at `path` (a `lifecycle.activation` field), refused
    /// by name when it is not `on_demand` or `explicit`.
    pub fn parse(value: &Value, path: &str) -> Result<Self, ConfigError> {
        match value.as_str() {
            Some("on_demand") => Ok(Self::OnDemand),
            Some("explicit") => Ok(Self::Explicit),
            _ => Err(invalid(path, "must be on_demand or explicit")),
        }
    }
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
    /// SPEC §6.5, §10: `lifecycle.activation`. Absent unless `explicit`, so
    /// a command that omits it, or states the default `on_demand`, keeps its
    /// idempotency fingerprint.
    #[serde(default, skip_serializing_if = "Activation::is_on_demand")]
    pub activation: Activation,
    /// ADR 0028 §2: the multi-node group this deployment runs as, present only
    /// for a world size above one, so a single-host command keeps its identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<crate::topology::GroupShape>,
    /// ADR 0035 §1, §1a: one instance on several GPUs of one host, or
    /// `topology: auto`. Absent on one GPU and for a group, so their commands
    /// keep their identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<crate::topology::LocalTopology>,
}

impl Default for InstanceSpec {
    fn default() -> Self {
        Self {
            instances: 1,
            placement: Placement::default(),
            warm: false,
            activation: Activation::OnDemand,
            group: None,
            local: None,
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
///
/// Owner decision 2026-09-25: this is where a deployment becomes one host's
/// document, so the host's defaults are filled first
/// ([`crate::deployment_defaults::for_host`]): the profile an engine family
/// names, its published revision, and the GPU when none is stated.
pub fn assign_devices(deployment: &Value, host: &Value) -> Result<Value, ConfigError> {
    let deployment = &crate::deployment_defaults::for_host(deployment, host)?;
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

/// ADR 0035 §1: the tolerance within which two GPUs' total memory is the
/// same, that of the ADR 0019 sample.
pub const SAME_TOTAL_TOLERANCE_BYTES: i64 = 64 << 20;

/// One discrete GPU of a host document, as eligibility reads it.
struct HostGpu {
    id: String,
    sharing: String,
    product: Option<String>,
    /// The card as the host policy declares it: the device domain's managed
    /// limit plus its free reserve.
    total: Option<i64>,
}

/// The driver index a `gpuN` id names, `u32::MAX` for any other id (so it
/// sorts last).
fn gpu_order(id: &str) -> u32 {
    id.strip_prefix("gpu")
        .and_then(|n| n.parse().ok())
        .unwrap_or(u32::MAX)
}

/// The host's GPUs whose domain is a `device` domain, lowest driver index
/// first, with the sharing each allows.
fn host_gpus(host: &Value) -> Vec<HostGpu> {
    let policy = &host["resource_policy"];
    let default_sharing = policy["device_sharing"].as_str().unwrap_or("exclusive");
    let bytes = |value: &Value| {
        value
            .as_str()
            .and_then(|text| crate::effective::parse_bytes(text).ok())
    };
    let mut gpus: Vec<HostGpu> = policy["devices"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(id, device)| {
            let domain = &policy["domains"][device["domain"].as_str()?];
            (domain["memory"] == "device").then(|| HostGpu {
                id: id.clone(),
                sharing: device["sharing"]
                    .as_str()
                    .unwrap_or(default_sharing)
                    .to_owned(),
                product: device["product_name"].as_str().map(str::to_owned),
                total: bytes(&domain["managed_limit"])
                    .zip(bytes(&domain["free_reserve"]))
                    .and_then(|(managed, reserve)| managed.checked_add(reserve)),
            })
        })
        .collect();
    gpus.sort_by(|a, b| {
        gpu_order(&a.id)
            .cmp(&gpu_order(&b.id))
            .then_with(|| a.id.cmp(&b.id))
    });
    gpus
}

/// ADR 0035 §1 (owner decision 4): two GPUs are the same product with the
/// same total memory, within [`SAME_TOTAL_TOLERANCE_BYTES`].
fn identical(a: &HostGpu, b: &HostGpu) -> bool {
    a.product == b.product
        && match (a.total, b.total) {
            (Some(x), Some(y)) => (x - y).abs() <= SAME_TOTAL_TOLERANCE_BYTES,
            _ => false,
        }
}

/// The number of discrete GPUs (device domains) a host document has.
pub fn discrete_gpu_count(host: &Value) -> usize {
    host_gpus(host).len()
}

/// ADR 0035 §1, §2: the host's sets of `n` identical discrete GPUs, each in
/// rank order (ascending driver index), the sets in lexicographic order of
/// their indices, so the first is the lowest. With `shared_only`, only GPUs
/// the host lets be shared. `n == 1` is every GPU on its own.
pub fn eligible_sets(host: &Value, n: usize, shared_only: bool) -> Vec<Vec<String>> {
    let gpus: Vec<HostGpu> = host_gpus(host)
        .into_iter()
        .filter(|gpu| !shared_only || gpu.sharing == "shared")
        .collect();
    let mut sets = Vec::new();
    for_each_combination(gpus.len(), n, &mut |pick| {
        let first = &gpus[pick[0]];
        if pick.iter().all(|&i| n == 1 || identical(first, &gpus[i])) {
            sets.push(pick.iter().map(|&i| gpus[i].id.clone()).collect());
        }
    });
    sets
}

/// Every `n`-combination of `0..len`, ascending within, in lexicographic
/// order; none when `n` is 0 or above `len`.
pub(crate) fn for_each_combination(len: usize, n: usize, visit: &mut dyn FnMut(&[usize])) {
    if n == 0 || n > len {
        return;
    }
    let mut pick: Vec<usize> = (0..n).collect();
    loop {
        visit(&pick);
        let Some(at) = (0..n).rev().find(|&k| pick[k] < len - n + k) else {
            return;
        };
        let base = pick[at] + 1;
        for (offset, slot) in pick[at..].iter_mut().enumerate() {
            *slot = base + offset;
        }
    }
}

/// ADR 0035 §2: the key a set of GPUs is chosen and recorded under, its ids
/// in rank order joined by `,` (one GPU's key is its id, as before).
pub fn set_key(ids: &[String]) -> String {
    ids.join(",")
}

/// The ids of a key [`set_key`] wrote, in rank order.
pub fn set_ids(key: &str) -> Vec<&str> {
    key.split(',').collect()
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
/// which name their domains and so their device (the short form
/// `{gpu, ram}` names none, so it offers the choice).
///
/// ADR 0035 §2: with `topology.tensor_parallel: N` on one host, each choice
/// is a set of N identical GPUs (no claim, or N unnamed claims of one
/// sharing), keyed by [`set_key`]. ADR 0035 §1a: under `topology: auto`
/// the choices are every candidate size's sets, N = 1 first, each the
/// deployment as an explicit `tensor_parallel: N` states it (no `topology`
/// at N = 1), so a chosen set launches exactly as the explicit N does;
/// placement picks N from the candidate table of the `auto` deployment's
/// own resolution ([`crate::effective::choose_tensor_parallel`]) and the
/// set among that N's choices.
pub fn device_choices(
    deployment: &Value,
    host: &Value,
) -> Result<Vec<(String, Value)>, ConfigError> {
    let names_domains = deployment
        .get("resources")
        .is_some_and(|r| !r.is_null() && !crate::short_resources::is_short(r));
    if host_gpus(host).is_empty() || names_domains {
        return Ok(Vec::new());
    }
    let claims = match deployment.get("devices") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(claims)) => claims.clone(),
        Some(_) => return Err(invalid("devices", "must be a list")),
    };
    let local = parse_instance_spec(deployment)?.local;
    let sizes: Vec<usize> = match local {
        None => vec![1],
        Some(crate::topology::LocalTopology::Fixed(topology)) => {
            vec![topology.tensor_parallel as usize]
        }
        Some(crate::topology::LocalTopology::Auto) => (1..=host_gpus(host).len()).collect(),
    };
    let wanted = match claims.as_slice() {
        [] => None,
        [claim, ..]
            if claims.len() == sizes[0]
                && claims
                    .iter()
                    .all(|c| c.get("id").is_none() && c["sharing"] == claim["sharing"]) =>
        {
            claim["sharing"].as_str()
        }
        _ => return Ok(Vec::new()),
    };
    let sharing_of = |id: &str| {
        host_gpus(host)
            .into_iter()
            .find(|gpu| gpu.id == id)
            .map(|gpu| gpu.sharing)
            .unwrap_or_default()
    };
    let mut choices = Vec::new();
    for n in sizes {
        for set in eligible_sets(host, n, wanted == Some("shared")) {
            let mut choice = deployment.clone();
            if local == Some(crate::topology::LocalTopology::Auto) {
                let object = choice.as_object_mut().expect("a deployment is a mapping");
                if n == 1 {
                    object.remove("topology");
                } else {
                    object.insert("topology".into(), serde_json::json!({"tensor_parallel": n}));
                }
            }
            choice["devices"] = Value::Array(
                set.iter()
                    .map(|id| {
                        serde_json::json!({
                            "id": id,
                            "sharing": wanted.map_or_else(|| sharing_of(id), str::to_owned),
                        })
                    })
                    .collect(),
            );
            choices.push((set_key(&set), choice));
        }
    }
    Ok(choices)
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
    // ADR 0030 §7: a decisions deployment's document-level refusals (one
    // machine, no parking), here so every path that validates the document's
    // shape refuses them first, before any group or placement rule.
    crate::decisions::check_document(deployment)?;
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
                // ADR 0028 §2: a group's repeat is a topology error.
                return Err(if crate::topology::declares_group(deployment) {
                    crate::topology::GroupRefusal::TopologyInvalid
                        .at("placement.hosts", "must not repeat a host")
                } else {
                    invalid("placement.hosts", "must not repeat a host")
                });
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
    // SPEC §6.5 (ADR 0013 amendment 2026-09-23): the warm commitment; SPEC
    // §6.5, §10: the activation policy.
    let mut warm = false;
    let mut activation = Activation::OnDemand;
    if let Some(raw) = object.get("lifecycle").filter(|v| !v.is_null()) {
        let raw = raw
            .as_object()
            .ok_or_else(|| invalid("lifecycle", "must be a mapping"))?;
        for key in raw.keys() {
            if key != "warm" && key != "activation" {
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
        if let Some(value) = raw.get("activation") {
            activation = Activation::parse(value, "lifecycle.activation")?;
        }
    }
    let mut spec = InstanceSpec {
        instances,
        placement,
        warm,
        activation,
        group: None,
        local: None,
    };
    spec.group = crate::topology::parse_group_shape(deployment, &spec)?;
    spec.local = crate::topology::parse_local_topology(deployment, &spec)?;
    if let Some(local) = spec.local {
        check_local_declaration(object, local)?;
    }
    // ADR 0013 §3: with an explicit allowed set the count must fit it. A group
    // places its ranks on its named hosts instead (ADR 0028 §2).
    if let Some(hosts) = spec
        .placement
        .hosts
        .as_ref()
        .filter(|_| spec.group.is_none())
    {
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
    // A group's ranks each hold one device on their own host (ADR 0028 §2), so
    // a device id there is that host's, and the rule is for the scheduler's
    // multi-host sets only.
    if names_devices
        && spec.group.is_none()
        && spec.placement.hosts.as_ref().is_some_and(|h| h.len() > 1)
    {
        return Err(invalid(
            "devices",
            "named devices require an allowed set of exactly one host",
        ));
    }
    Ok(spec)
}

/// ADR 0035 §1, §1a, §3: what a deployment on several GPUs of one host may
/// state beside its `topology`. `devices` pins exactly N distinct GPUs (no
/// claim, or N claims); beside `auto` it pins none, and declares no
/// `resources`, `memory.request` or `memory.startup`: the charge is derived
/// for each candidate N, and a fixed per-GPU figure would mean something
/// different at each.
fn check_local_declaration(
    object: &serde_json::Map<String, Value>,
    local: crate::topology::LocalTopology,
) -> Result<(), ConfigError> {
    use crate::topology::{LocalTopology, TOPOLOGY_DEVICES_MISMATCH};
    let claims = object
        .get("devices")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    match local {
        LocalTopology::Auto => {
            if !claims.is_empty() {
                return Err(invalid(
                    "devices",
                    &format!(
                        "{TOPOLOGY_DEVICES_MISMATCH}: `topology: auto` chooses its GPUs; to pin \
                         GPUs, state `topology: {{tensor_parallel: N}}` with N devices"
                    ),
                ));
            }
            let memory = object
                .get("engine_config")
                .and_then(|config| config.get("memory"));
            let stated =
                |key: &str| memory.is_some_and(|m| m.get(key).is_some_and(|v| !v.is_null()));
            for (path, stated) in [
                (
                    "resources",
                    object.get("resources").is_some_and(|r| !r.is_null()),
                ),
                ("engine_config.memory.request", stated("request")),
                ("engine_config.memory.startup", stated("startup")),
            ] {
                if stated {
                    return Err(invalid(
                        path,
                        &format!(
                            "`{path}` beside `topology: auto`: the charge is derived for each \
                             candidate tensor-parallel size, and a fixed per-GPU figure would \
                             mean something different at each; remove it, or state \
                             `topology: {{tensor_parallel: N}}`"
                        ),
                    ));
                }
            }
        }
        LocalTopology::Fixed(topology) => {
            let named: BTreeSet<&str> = claims
                .iter()
                .filter_map(|claim| claim.get("id").and_then(Value::as_str))
                .collect();
            let distinct = named.len() == claims.len() || named.is_empty();
            if !claims.is_empty()
                && (claims.len() != topology.tensor_parallel as usize || !distinct)
            {
                return Err(invalid(
                    "devices",
                    &format!(
                        "{TOPOLOGY_DEVICES_MISMATCH}: `topology.tensor_parallel: {n}` runs on {n} \
                         distinct GPUs; `devices` lists {} {}",
                        claims.len(),
                        if distinct {
                            "entries"
                        } else {
                            "entries with a repeat"
                        },
                        n = topology.tensor_parallel,
                    ),
                ));
            }
        }
    }
    Ok(())
}
