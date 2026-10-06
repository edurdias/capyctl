//! ADR 0028 §10: the engine-neutral arguments of one multi-node group member
//! ([`GroupMemberArgs`]), taken from the group plan. Each engine adapter
//! renders them in its own spelling; the protected entry compares the parse
//! against them before serving.

use capyctl_domain::group::{GroupMemberArgs, GroupPlan};

/// The environment variable that turns a protected entry into group mode.
pub const GROUP_MODE_ENV: &str = "CAPYCTL_GROUP_MODE";
/// The environment variable carrying the expected multi-node destinations.
pub const GROUP_EXPECTED_ENV: &str = "CAPYCTL_GROUP_EXPECTED";
/// ADR 0028 §10 (R11): the only transport variable a group launch renders.
pub const GLOO_SOCKET_IFNAME: &str = "GLOO_SOCKET_IFNAME";
/// ADR 0028 §10: the SGLang member's own peer address variable.
pub const SGLANG_HOST_IP: &str = "SGLANG_HOST_IP";

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
