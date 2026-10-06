//! ADR 0028 §10: the engine-neutral arguments of one multi-node group member
//! ([`GroupMemberArgs`]), taken from the group plan. Each engine adapter
//! renders them in its own spelling; the protected entry compares the parse
//! against them before serving.

use crate::traits::{OwnedProcessLaunch, RuntimeError};
use capyctl_domain::completion::{
    EffectObservation, Presence, ProcessIdentity, StepExecutionContext,
};
use capyctl_domain::group::{GroupMemberArgs, GroupPlan};
use std::sync::Arc;

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

/// ADR 0028 §9, §10: the end of a group worker's Initialize, shared by every
/// engine. Readiness is the head's, so nothing here talks to the worker (a
/// vLLM headless worker and a TensorFold follower serve nothing; SGLang's
/// rank > 0 health server always passes). The step reports the recorded
/// process tree once the spawned process `api` is present, and claims no
/// milestone: a worker alone serves nothing. `failed` is the engine's own
/// launch failure for a process that already left.
pub(crate) async fn worker_spawned(
    context: &StepExecutionContext,
    tools: Arc<dyn OwnedProcessLaunch>,
    api: ProcessIdentity,
    builds: crate::kernel_builds::BuildWatch,
    failed: impl FnOnce() -> RuntimeError,
    receipt: String,
) -> Result<EffectObservation, RuntimeError> {
    let presence_tools = tools.clone();
    let watched = api.clone();
    match tokio::task::spawn_blocking(move || presence_tools.present(&watched))
        .await
        .map_err(|_| RuntimeError::Uncertain("presence task failed".into()))?
    {
        Presence::Alive => {}
        Presence::Gone => return Err(failed()),
        Presence::Unknown => {
            return Err(RuntimeError::Uncertain(
                "group worker presence could not be established".into(),
            ))
        }
    }
    let led_by = api.clone();
    let identities = tokio::task::spawn_blocking(move || tools.observe_group(&led_by))
        .await
        .map_err(|_| RuntimeError::Uncertain("group task failed".into()))??;
    // The recorded tree must lead with the process this step spawned.
    if identities.first() != Some(&api) {
        return Err(RuntimeError::Uncertain(format!(
            "group worker tree does not lead with its spawned process: {}",
            identities
                .iter()
                .map(|i| format!("{}:{}", i.role, i.pid))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    Ok(EffectObservation {
        token: context.token.clone(),
        binding_id: context.binding_id.clone(),
        incarnation: context.incarnation.clone(),
        identities,
        observed_at_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| RuntimeError::Uncertain("system clock is before the epoch".into()))?
            .as_millis() as i64,
        receipt,
        facts: Vec::new(),
        kernel_builds: builds.finish(),
    })
}
