//! ADR 0028 §2: a deployment's multi-node topology. A world size above one
//! requires an exact, rank-ordered host list; the first host is the head.
use crate::{instances::InstanceSpec, ConfigError, ConfigErrorCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Topology {
    pub tensor_parallel: u32,
    pub pipeline_parallel: u32,
}

impl Topology {
    pub fn world_size(&self) -> u32 {
        self.tensor_parallel * self.pipeline_parallel
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupShape {
    pub hosts: Vec<String>,
    pub topology: Topology,
    pub local_ranks: u32,
}

impl GroupShape {
    pub fn head(&self) -> &str {
        &self.hosts[0]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupRefusal {
    PlacementRequired,
    TopologyInvalid,
    ShapeUnsupported,
    InstancesUnsupported,
}

impl GroupRefusal {
    pub fn code(&self) -> &'static str {
        match self {
            Self::PlacementRequired => "group_placement_required",
            Self::TopologyInvalid => "group_topology_invalid",
            Self::ShapeUnsupported => "group_shape_unsupported",
            Self::InstancesUnsupported => "group_instances_unsupported",
        }
    }

    pub(crate) fn at(self, path: &str, detail: &str) -> ConfigError {
        ConfigError::new(
            ConfigErrorCode::UnsupportedCombination,
            path,
            format!("{}: {detail}", self.code()),
        )
    }
}

/// Far above any hardware in hand, low enough that `world_size` never overflows.
const MAX_DIMENSION: u64 = 64;

fn dimension(raw: &Value, key: &str) -> Result<u32, ConfigError> {
    match raw.get(key) {
        None | Some(Value::Null) => Ok(1),
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=MAX_DIMENSION).contains(n))
            .map(|n| n as u32)
            .ok_or_else(|| {
                GroupRefusal::TopologyInvalid.at(&format!("topology.{key}"), "must be 1..=64")
            }),
    }
}

/// Whether the document declares a topology whose world size is above one.
/// Lenient: a malformed dimension is reported by [`parse_group_shape`].
pub(crate) fn declares_group(deployment: &Value) -> bool {
    let Some(raw) = deployment.get("topology").filter(|v| !v.is_null()) else {
        return false;
    };
    let dim = |key: &str| raw.get(key).and_then(Value::as_u64).unwrap_or(1);
    dim("tensor_parallel").saturating_mul(dim("pipeline_parallel")) > 1
}

pub fn parse_group_shape(
    deployment: &Value,
    spec: &InstanceSpec,
) -> Result<Option<GroupShape>, ConfigError> {
    let Some(raw) = deployment.get("topology").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let topology = Topology {
        tensor_parallel: dimension(raw, "tensor_parallel")?,
        pipeline_parallel: dimension(raw, "pipeline_parallel")?,
    };
    // ADR 0028 §2: world size 1 is the single-host path, no group rules.
    if topology.world_size() == 1 {
        return Ok(None);
    }
    // ADR 0028 §2 (owner decision 8): named hosts only, head first.
    let placement = &deployment["placement"];
    if deployment.get("host").is_some_and(|v| !v.is_null())
        || ["selector", "strategy", "max_per_host"]
            .iter()
            .any(|k| placement.get(*k).is_some_and(|v| !v.is_null()))
    {
        return Err(GroupRefusal::PlacementRequired
            .at("placement", "a group lists exact hosts only, head first"));
    }
    let Some(hosts) = spec.placement.hosts.clone().filter(|h| h.len() >= 2) else {
        return Err(GroupRefusal::PlacementRequired.at(
            "placement.hosts",
            "a group names at least two hosts, head first",
        ));
    };
    if hosts.iter().any(|h| h.trim() != h) {
        return Err(GroupRefusal::TopologyInvalid
            .at("placement.hosts", "host names have no surrounding space"));
    }
    if spec.instances != 1 {
        return Err(GroupRefusal::InstancesUnsupported.at(
            "instances",
            "a group deployment has one instance in this version",
        ));
    }
    let n = hosts.len() as u32;
    if !topology.world_size().is_multiple_of(n) {
        return Err(GroupRefusal::TopologyInvalid.at(
            "topology",
            "the host count must divide tensor_parallel x pipeline_parallel",
        ));
    }
    let local_ranks = topology.world_size() / n;
    if local_ranks != 1 {
        return Err(
            GroupRefusal::ShapeUnsupported.at("topology", "one device per host in this version")
        );
    }
    Ok(Some(GroupShape {
        hosts,
        topology,
        local_ranks,
    }))
}

/// ADR 0028 §2: standalone is one host, so it cannot run a group.
pub fn refuse_in_standalone(deployment: &Value) -> Result<(), ConfigError> {
    if declares_group(deployment) {
        return Err(GroupRefusal::PlacementRequired.at(
            "topology",
            "standalone is one host; a group needs at least two",
        ));
    }
    Ok(())
}
