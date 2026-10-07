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

/// ADR 0028 §16: the closed deploy-time refusals of a group deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupRefusal {
    PlacementRequired,
    TopologyInvalid,
    ShapeUnsupported,
    InstancesUnsupported,
    /// The profile does not resolve on a named host, or its build differs.
    ProfileMismatch,
    /// A named host declares no peer address (ADR 0028 §3).
    PeerAddressMissing,
}

impl GroupRefusal {
    pub fn code(&self) -> &'static str {
        match self {
            Self::PlacementRequired => "group_placement_required",
            Self::TopologyInvalid => "group_topology_invalid",
            Self::ShapeUnsupported => "group_shape_unsupported",
            Self::InstancesUnsupported => "group_instances_unsupported",
            Self::ProfileMismatch => "group_profile_mismatch",
            Self::PeerAddressMissing => "peer_address_missing",
        }
    }

    /// The refusal as a configuration error at `path`, its detail led by the
    /// closed code.
    pub fn at(self, path: &str, detail: &str) -> ConfigError {
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

/// Whether the document declares a topology that is not the single-host one:
/// any stated dimension other than 1, valid or not, so a malformed group is
/// refused with the group codes. [`parse_group_shape`] reports the dimension.
pub(crate) fn declares_group(deployment: &Value) -> bool {
    let Some(raw) = deployment.get("topology").filter(|v| !v.is_null()) else {
        return false;
    };
    ["tensor_parallel", "pipeline_parallel"].iter().any(|key| {
        raw.get(*key)
            .is_some_and(|v| !v.is_null() && v.as_u64() != Some(1))
    })
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

/// ADR 0028 §3: a host named in a group declares the peer address its peers
/// reach it on (read from the host document's groups block, never from the
/// normalized policy). Deploy and `validate config --host` both ask this.
pub fn check_member_peer_address(name: &str, host: &Value) -> Result<(), ConfigError> {
    if crate::groups_policy::host_groups_policy(host)?
        .peer_address
        .is_none()
    {
        return Err(GroupRefusal::PeerAddressMissing.at(
            "placement.hosts",
            &format!("host `{name}` declares no resource_policy.groups.peer_address"),
        ));
    }
    Ok(())
}

/// ADR 0028 §2, spec §16: why one named host of a group did not resolve. A
/// profile that does not resolve there is `group_profile_mismatch`; any other
/// configuration reason (an engine env name not approved on that host,
/// `engine_env_not_approved:<name>`) is named as it is.
pub fn member_resolution_error(host: &str, error: ConfigError) -> ConfigError {
    if matches!(
        error.path.as_str(),
        "runtime_profile" | "runtime_profile_revision"
    ) {
        return GroupRefusal::ProfileMismatch.at(
            &error.path,
            &format!("the runtime profile does not resolve on host `{host}`"),
        );
    }
    error
}

/// ADR 0028 §2: the members of one group run one build (else
/// `group_profile_mismatch`), and the decision every host makes before a
/// launch, Park or Restore (`EffectiveDeployment::deep_wake_refusal`, the
/// capability gate) admits every member's resolution: one member refused
/// refuses the group. `members` is each host with its resolution, in rank
/// order, the head first.
pub fn check_group_members(
    members: &[(&str, &crate::effective::EffectiveDeployment)],
) -> Result<(), ConfigError> {
    let Some((head_host, head)) = members.first() else {
        return Ok(());
    };
    for (host, member) in members {
        if member.profile.engine != head.profile.engine
            || member.profile.build_fingerprint != head.profile.build_fingerprint
        {
            return Err(GroupRefusal::ProfileMismatch.at(
                "runtime_profile",
                &format!(
                    "the runtime profile's build on host `{host}` differs from the head's on `{head_host}`"
                ),
            ));
        }
        if let Some(reason) = member.deep_wake_refusal() {
            return Err(ConfigError::new(
                ConfigErrorCode::UnsupportedCombination,
                "residency",
                format!("{reason}: host `{host}` cannot park and wake this group member"),
            ));
        }
    }
    Ok(())
}
