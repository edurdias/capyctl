//! SPEC §§11 and 13: process identities are local to an assigned host/member.
use crate::completion::ProcessIdentity;
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct MemberKey {
    pub host_id: String,
    pub member_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberProcesses {
    pub member: MemberKey,
    pub processes: Vec<ProcessIdentity>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid or incomplete host-scoped group evidence")]
pub struct GroupIdentityError;

/// Validate local identities without assuming every member has an API process.
/// A headless rank is valid; roles and PIDs are unique only within this host.
pub fn validate_local_processes(ids: &[ProcessIdentity]) -> Result<(), GroupIdentityError> {
    let Some(first) = ids.first() else {
        return Err(GroupIdentityError);
    };
    let mut roles = BTreeSet::new();
    let mut pids = BTreeSet::new();
    if ids.iter().any(|id| {
        id.role.trim().is_empty()
            || id.pid == 0
            || id.start_ticks == 0
            || id.boot_id.trim().is_empty()
            || id.boot_id != first.boot_id
            || !roles.insert(&id.role)
            || !pids.insert(id.pid)
    }) {
        return Err(GroupIdentityError);
    }
    Ok(())
}

/// SPEC §11: every assigned member must provide the exact recorded identities.
/// PID reuse never becomes completion evidence, and missing ranks cannot settle.
pub fn verify_group_processes(
    expected: &[MemberProcesses],
    actual: &[MemberProcesses],
) -> Result<(), GroupIdentityError> {
    fn canonical(
        members: &[MemberProcesses],
    ) -> Result<std::collections::BTreeMap<MemberKey, BTreeSet<ProcessIdentity>>, GroupIdentityError>
    {
        if members.is_empty() {
            return Err(GroupIdentityError);
        }
        let mut result = std::collections::BTreeMap::new();
        let mut processes = BTreeSet::new();
        let mut boots = std::collections::BTreeMap::new();
        for member in members {
            if member.member.host_id.trim().is_empty() || member.member.member_id.trim().is_empty()
            {
                return Err(GroupIdentityError);
            }
            validate_local_processes(&member.processes)?;
            if boots
                .insert(&member.member.host_id, &member.processes[0].boot_id)
                .is_some_and(|boot| boot != &member.processes[0].boot_id)
            {
                return Err(GroupIdentityError);
            }
            for process in &member.processes {
                if !processes.insert((&member.member.host_id, process.pid)) {
                    return Err(GroupIdentityError);
                }
            }
            if result
                .insert(
                    member.member.clone(),
                    member.processes.iter().cloned().collect(),
                )
                .is_some()
            {
                return Err(GroupIdentityError);
            }
        }
        Ok(result)
    }
    if canonical(expected)? != canonical(actual)? {
        return Err(GroupIdentityError);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberRole {
    Head,
    Worker,
}

/// ADR 0028 §4: the engines a multi-node group can run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupEngine {
    Vllm,
    Sglang,
    Tensorfold,
}
impl GroupEngine {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Vllm => "vllm",
            Self::Sglang => "sglang",
            Self::Tensorfold => "tensorfold",
        }
    }
}

/// ADR 0028 §4: the parallelism the group spans. `local_ranks` is the number of
/// devices (ranks) each member contributes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GroupTopology {
    pub tensor_parallel: u32,
    pub pipeline_parallel: u32,
    pub local_ranks: u32,
}

/// ADR 0028 §4: the member id of a rank: `head` for rank 0, `worker-<r>` after.
pub fn member_id(rank: u32) -> String {
    if rank == 0 {
        "head".into()
    } else {
        format!("worker-{rank}")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberPlan {
    pub member: MemberKey,
    pub rank: u32,
    pub role: MemberRole,
    pub profile_name: String,
    pub profile_fingerprint: String,
    pub checkpoint_fingerprint: String,
    /// R7: this host's own path of the materialized source.
    pub model_path: String,
    pub devices: Vec<String>,
    pub peer_address: std::net::IpAddr,
    /// Only the head serves the API.
    pub service_port: Option<u16>,
    /// The loopback port of a SGLang worker; no other member has one.
    pub worker_port: Option<u16>,
}

/// ADR 0028 §10: the engine-neutral arguments of one multi-node group member,
/// taken from the group plan (`capyctl_adapters::group::member_args`). Each
/// engine adapter renders them in its own spelling; the protected entry
/// compares the parse against them before serving. It lives here so a frozen
/// launch (`crate::launch::NativeLaunch`) can carry it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupMemberArgs {
    pub tensor_parallel: u32,
    pub pipeline_parallel: u32,
    pub nnodes: u32,
    pub node_rank: u32,
    /// The head's peer address: the engine's rendezvous address.
    pub head_address: std::net::IpAddr,
    pub rendezvous_port: u16,
    /// This member's own peer address.
    pub own_address: std::net::IpAddr,
    /// A SGLang worker's loopback port; `None` for every other member.
    pub worker_port: Option<u16>,
    /// ADR 0028 §10 (R11): the interface holding `own_address`, rendered as
    /// `GLOO_SOCKET_IFNAME`. The plan cannot know it; the agent fills it at
    /// launch and refuses the launch if it cannot.
    pub own_interface: Option<String>,
}

impl GroupMemberArgs {
    /// Rank 0 is the head: the only member that serves the API.
    pub fn is_head(&self) -> bool {
        self.node_rank == 0
    }

    /// The `CAPYCTL_GROUP_EXPECTED` payload: the engine's own destination
    /// `fields` (a JSON object), plus `gloo_socket_ifname` when rendered.
    pub fn expected_json(&self, fields: serde_json::Value) -> String {
        let mut object = match fields {
            serde_json::Value::Object(object) => object,
            _ => serde_json::Map::new(),
        };
        if let Some(interface) = &self.own_interface {
            object.insert("gloo_socket_ifname".into(), interface.clone().into());
        }
        serde_json::Value::Object(object).to_string()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupPlan {
    engine: GroupEngine,
    members: Vec<MemberPlan>,
    topology: GroupTopology,
    rendezvous_port: u16,
    generation: i64,
}
impl GroupPlan {
    /// ADR 0028 §4: an N-member plan, ranks in order, one head, one engine
    /// recipe and checkpoint across members, and a topology the devices cover.
    /// Recipe qualification remains a separate prerequisite; shape grants no
    /// launch authority.
    pub fn new(
        engine: GroupEngine,
        members: Vec<MemberPlan>,
        topology: GroupTopology,
        rendezvous_port: u16,
        generation: i64,
    ) -> Result<Self, GroupIdentityError> {
        if members.len() < 2
            || rendezvous_port == 0
            || generation <= 0
            || topology.tensor_parallel == 0
            || topology.pipeline_parallel == 0
            || topology.local_ranks == 0
        {
            return Err(GroupIdentityError);
        }
        let mut hosts = BTreeSet::new();
        let mut peers = BTreeSet::new();
        for (index, member) in members.iter().enumerate() {
            let rank = index as u32;
            let head = index == 0;
            let worker_port_ok = if head {
                member.worker_port.is_none()
            } else if engine == GroupEngine::Sglang {
                member.worker_port.is_some_and(|port| port != 0)
            } else {
                member.worker_port.is_none()
            };
            let service_port_ok = if head {
                member.service_port.is_some_and(|port| port != 0)
            } else {
                member.service_port.is_none()
            };
            if member.rank != rank
                || (member.role == MemberRole::Head) != head
                || member.member.member_id != member_id(rank)
                || member.member.host_id.trim().is_empty()
                || !hosts.insert(&member.member.host_id)
                || member.profile_name.trim().is_empty()
                || member.profile_fingerprint.trim().is_empty()
                || member.checkpoint_fingerprint.trim().is_empty()
                || member.model_path.trim().is_empty()
                || member.devices.len() != topology.local_ranks as usize
                || member.devices.iter().any(|d| d.trim().is_empty())
                || !service_port_ok
                || !worker_port_ok
                || member.peer_address.is_unspecified()
                || member.peer_address.is_multicast()
                || member.peer_address.is_loopback()
                || !peers.insert(member.peer_address)
                || member.profile_fingerprint != members[0].profile_fingerprint
                || member.checkpoint_fingerprint != members[0].checkpoint_fingerprint
            {
                return Err(GroupIdentityError);
            }
        }
        let ranks = members.len() as u64 * u64::from(topology.local_ranks);
        if u64::from(topology.tensor_parallel) * u64::from(topology.pipeline_parallel) != ranks
            || (engine == GroupEngine::Tensorfold
                && (members.len() != 2
                    || topology.tensor_parallel != 2
                    || topology.pipeline_parallel != 1))
        {
            return Err(GroupIdentityError);
        }
        Ok(Self {
            engine,
            members,
            topology,
            rendezvous_port,
            generation,
        })
    }
    pub fn engine(&self) -> GroupEngine {
        self.engine
    }
    pub fn members(&self) -> &[MemberPlan] {
        &self.members
    }
    pub fn head(&self) -> &MemberPlan {
        &self.members[0]
    }
    pub fn topology(&self) -> GroupTopology {
        self.topology
    }
    pub fn rendezvous_port(&self) -> u16 {
        self.rendezvous_port
    }
    pub fn generation(&self) -> i64 {
        self.generation
    }
}

/// SPEC §13.1: identity is per lifecycle step, not merely per operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandIdentity {
    pub controller_id: String,
    pub member: MemberKey,
    pub deployment_id: String,
    pub operation_id: String,
    pub command_id: String,
    pub step_id: String,
    pub generation: i64,
    pub revision: i64,
    pub deadline_ms: i64,
    pub payload_digest: [u8; 32],
    pub expected_state: String,
    pub profile_fingerprint: String,
    /// ADR 0013 §5: the deployment instance this command belongs to. A host
    /// fences commands per (deployment, instance), so two instances of one
    /// deployment on one host never fence each other. Zero for instance 0 and
    /// for every command written before instances existed.
    pub instance_index: u32,
}
/// ADR 0013 §2: a deployment has at most 64 instances, indexed `0..64`.
pub const MAX_INSTANCE_INDEX: u32 = 63;
impl CommandIdentity {
    pub fn validate(&self) -> Result<(), GroupIdentityError> {
        if [
            &self.controller_id,
            &self.member.host_id,
            &self.member.member_id,
            &self.deployment_id,
            &self.operation_id,
            &self.command_id,
            &self.step_id,
            &self.expected_state,
            &self.profile_fingerprint,
        ]
        .iter()
        .any(|s| s.trim().is_empty() || s.len() > 256)
            || self.generation <= 0
            || self.revision <= 0
            || self.deadline_ms <= 0
            || self.instance_index > MAX_INSTANCE_INDEX
        {
            return Err(GroupIdentityError);
        }
        Ok(())
    }
}
