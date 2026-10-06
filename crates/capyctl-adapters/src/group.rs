//! ADR 0028 §10: the engine-neutral arguments of one multi-node group member,
//! taken from the group plan. Each engine adapter renders them in its own
//! spelling; the protected entry compares the parse against
//! [`GroupMemberArgs::expected_json`] before serving.

use std::net::IpAddr;

use capyctl_domain::group::GroupPlan;

/// The environment variable that turns a protected entry into group mode.
pub const GROUP_MODE_ENV: &str = "CAPYCTL_GROUP_MODE";
/// The environment variable carrying the expected multi-node destinations.
pub const GROUP_EXPECTED_ENV: &str = "CAPYCTL_GROUP_EXPECTED";
/// ADR 0028 §10 (R11): the only transport variable a group launch renders.
pub const GLOO_SOCKET_IFNAME: &str = "GLOO_SOCKET_IFNAME";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupMemberArgs {
    pub tensor_parallel: u32,
    pub pipeline_parallel: u32,
    pub nnodes: u32,
    pub node_rank: u32,
    /// The head's peer address: the engine's rendezvous address.
    pub head_address: IpAddr,
    pub rendezvous_port: u16,
    /// This member's own peer address.
    pub own_address: IpAddr,
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

/// The arguments of the member `host_id` runs, or `None` when the host holds
/// no member of `plan`. `own_interface` is left `None` (filled at launch).
pub fn member_args(plan: &GroupPlan, host_id: &str) -> Option<GroupMemberArgs> {
    let member = plan
        .members()
        .iter()
        .find(|member| member.member.host_id == host_id)?;
    let topology = plan.topology();
    Some(GroupMemberArgs {
        tensor_parallel: topology.tensor_parallel,
        pipeline_parallel: topology.pipeline_parallel,
        nnodes: plan.members().len() as u32,
        node_rank: member.rank,
        head_address: plan.head().peer_address,
        rendezvous_port: plan.rendezvous_port(),
        own_address: member.peer_address,
        worker_port: member.worker_port,
        own_interface: None,
    })
}
