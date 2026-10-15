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

/// ADR 0028 §8, §11: how often a group worker's process tree is scanned while
/// it settles after the spawn.
const WORKER_TREE_POLL: std::time::Duration = std::time::Duration::from_millis(100);
/// ADR 0028 §8, §11: how long the tree must stay unchanged before it is
/// reported: an engine forks its helpers right after it starts, and a child
/// missing from the reply would be missing from the server's record too.
const WORKER_TREE_SETTLE: std::time::Duration = std::time::Duration::from_secs(1);
/// ADR 0028 §8: the longest the settling waits once the rank processes are
/// up. A tree still changing then is reported as last scanned; every scan is
/// journaled, and the journal-owned Terminate also signals the recorded
/// group's later members.
const WORKER_TREE_BOUND: std::time::Duration = std::time::Duration::from_secs(10);
/// The settling ends this far ahead of the step's deadline.
const WORKER_TREE_MARGIN_MS: i64 = 2_000;

/// ADR 0028 §9, §10: the end of a group worker's Initialize, shared by every
/// engine. Readiness is the head's, so nothing here talks to the worker (a
/// vLLM headless worker and a TensorFold follower serve nothing; SGLang's
/// rank > 0 health server always passes). The step claims no milestone: a
/// worker alone serves nothing. `failed` is the engine's own launch failure
/// for a process that already left.
///
/// The spec's launch contract (2026-10-05 design, "worker: its process
/// tree"): the reply names the processes holding the member's devices. Found
/// live 2026-10-15 (MN4, MN5): a vLLM headless worker forks `VLLM::Worker_TP1`,
/// and a SGLang rank > 0 its scheduler, only once the engine imported, seconds
/// after the spawn, so a tree settled one second after the spawn named the
/// leader alone. The member's park evidence (its own host's
/// `process_residency`, its scheduler's saver enrollment) and its cleanup
/// then never knew the process holding the GPU. So the step first waits,
/// bounded by its deadline, until the tree holds `rank_processes` processes
/// besides the spawned one (the member's local ranks; none for a TensorFold
/// follower, which serves from one process), then reports it once it has
/// stayed unchanged for [`WORKER_TREE_SETTLE`] (bounded by
/// [`WORKER_TREE_BOUND`]). A worker whose ranks never start by the deadline
/// is uncertain, never reported as if whole.
pub(crate) async fn worker_spawned(
    context: &StepExecutionContext,
    tools: Arc<dyn OwnedProcessLaunch>,
    api: ProcessIdentity,
    rank_processes: usize,
    builds: crate::kernel_builds::BuildWatch,
    failed: impl FnOnce() -> RuntimeError,
    receipt: String,
) -> Result<EffectObservation, RuntimeError> {
    let now_ms = || -> Result<i64, RuntimeError> {
        Ok(std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| RuntimeError::Uncertain("system clock is before the epoch".into()))?
            .as_millis() as i64)
    };
    let present = |tools: Arc<dyn OwnedProcessLaunch>, api: ProcessIdentity| async move {
        tokio::task::spawn_blocking(move || tools.present(&api))
            .await
            .map_err(|_| RuntimeError::Uncertain("presence task failed".into()))
    };
    let scan = |tools: Arc<dyn OwnedProcessLaunch>, api: ProcessIdentity| async move {
        tokio::task::spawn_blocking(move || tools.observe_group(&api))
            .await
            .map_err(|_| RuntimeError::Uncertain("group task failed".into()))?
    };
    let mut failed = Some(failed);
    let mut gone = || {
        failed
            .take()
            .map_or(RuntimeError::Unsupported, |failed| failed())
    };
    let mut alive =
        async |tools: Arc<dyn OwnedProcessLaunch>, api: ProcessIdentity| match present(tools, api)
            .await?
        {
            Presence::Alive => Ok(()),
            Presence::Gone => Err(gone()),
            Presence::Unknown => Err(RuntimeError::Uncertain(
                "group worker presence could not be established".into(),
            )),
        };
    alive(tools.clone(), api.clone()).await?;
    let stop_at = context.deadline_ms.saturating_sub(WORKER_TREE_MARGIN_MS);
    let ranks_up =
        |tree: &[ProcessIdentity]| tree.iter().filter(|p| **p != api).count() >= rank_processes;
    // Every scan is journaled by the tools; the reply carries the last one.
    let mut identities = scan(tools.clone(), api.clone()).await?;
    while !ranks_up(&identities) {
        if now_ms()? >= stop_at {
            return Err(RuntimeError::Uncertain(format!(
                "group worker started {} of its {rank_processes} rank processes by the deadline",
                identities.iter().filter(|p| **p != api).count()
            )));
        }
        tokio::time::sleep(WORKER_TREE_POLL).await;
        alive(tools.clone(), api.clone()).await?;
        identities = scan(tools.clone(), api.clone()).await?;
    }
    // The ranks are up: report the tree once it stopped changing.
    let started = std::time::Instant::now();
    let mut unchanged_since = std::time::Instant::now();
    while unchanged_since.elapsed() < WORKER_TREE_SETTLE
        && started.elapsed() < WORKER_TREE_BOUND
        && now_ms()? < stop_at
    {
        tokio::time::sleep(WORKER_TREE_POLL).await;
        alive(tools.clone(), api.clone()).await?;
        let current = scan(tools.clone(), api.clone()).await?;
        if current != identities {
            identities = current;
            unchanged_since = std::time::Instant::now();
        }
    }
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
        observed_at_ms: now_ms()?,
        receipt,
        facts: Vec::new(),
        kernel_builds: builds.finish(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use capyctl_domain::completion::{ExecutionIdentities, TransitionToken};
    use std::time::{Duration, Instant};

    fn identity(role: &str, pid: u32) -> ProcessIdentity {
        ProcessIdentity {
            role: role.into(),
            pid,
            boot_id: "boot".into(),
            start_ticks: u64::from(pid),
        }
    }

    /// A worker whose engine forks its rank process `after` its spawn, as a
    /// vLLM headless worker forks `VLLM::Worker_TP1` (and SGLang its rank's
    /// scheduler) once its imports are done.
    struct LateRank {
        spawned: Instant,
        after: Duration,
    }

    impl OwnedProcessLaunch for LateRank {
        fn spawn_durable(
            &self,
            _: &str,
            _: &crate::traits::RenderedCommand,
        ) -> Result<ProcessIdentity, RuntimeError> {
            Err(RuntimeError::Unsupported)
        }
        fn present(&self, _: &ProcessIdentity) -> Presence {
            Presence::Alive
        }
        fn observe_group(&self, _: &ProcessIdentity) -> Result<Vec<ProcessIdentity>, RuntimeError> {
            let mut tree = vec![identity("api", 10)];
            if self.spawned.elapsed() >= self.after {
                tree.push(identity("worker-0", 11));
            }
            Ok(tree)
        }
        fn terminate_owned(&self, _: &[ProcessIdentity], _: Duration) -> Result<(), RuntimeError> {
            Ok(())
        }
    }

    fn context(deadline_in: Duration) -> StepExecutionContext {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        StepExecutionContext {
            token: TransitionToken {
                deployment_id: "deployment".into(),
                revision: 1,
                generation: 1,
                operation_id: "operation".into(),
                step_id: "step".into(),
            },
            binding_id: "binding".into(),
            incarnation: "incarnation".into(),
            issued_at_ms: now,
            deadline_ms: now + deadline_in.as_millis() as i64,
            identities: ExecutionIdentities::OwnedLaunch,
            completion_target: None,
            grant_id: None,
            launch_settings: None,
        }
    }

    async fn spawned(
        tools: Arc<LateRank>,
        ranks: usize,
        deadline_in: Duration,
    ) -> Result<EffectObservation, RuntimeError> {
        let api = identity("api", 10);
        let builds = crate::kernel_builds::BuildWatch::start(tools.clone(), api.clone());
        worker_spawned(
            &context(deadline_in),
            tools,
            api,
            ranks,
            builds,
            || RuntimeError::LaunchFailed("gone".into()),
            "receipt".into(),
        )
        .await
    }

    // T31, T22 (ADR 0028 §8, §12; found live 2026-10-15, MN4 and MN5): a rank
    // process forked after the tree would have settled is in the worker's
    // Launch reply, so its host's residency report and its scheduler's saver
    // enrollment name a recorded process of the member. Failed before the
    // change: the reply named the leader alone.
    #[tokio::test]
    async fn a_worker_reports_its_rank_process_forked_after_its_imports() {
        let tools = Arc::new(LateRank {
            spawned: Instant::now(),
            after: WORKER_TREE_SETTLE + Duration::from_millis(1500),
        });
        let observed = spawned(tools, 1, Duration::from_secs(60)).await.unwrap();
        assert_eq!(
            observed.identities,
            [identity("api", 10), identity("worker-0", 11)]
        );
        assert!(observed.facts.is_empty(), "a worker claims no milestone");
    }

    // T31 (ADR 0028 §8): a worker whose rank processes never start by its
    // deadline is uncertain, never reported as a whole tree; a follower that
    // serves from one process (TensorFold) waits for none.
    #[tokio::test]
    async fn a_worker_without_its_ranks_by_the_deadline_is_uncertain() {
        let never = || {
            Arc::new(LateRank {
                spawned: Instant::now(),
                after: Duration::from_secs(3600),
            })
        };
        let deadline = Duration::from_millis(u64::try_from(WORKER_TREE_MARGIN_MS).unwrap() + 500);
        let refused = spawned(never(), 1, deadline).await;
        assert!(
            matches!(&refused, Err(RuntimeError::Uncertain(reason)) if reason.contains("0 of its 1 rank")),
            "{refused:?}"
        );
        let follower = spawned(never(), 0, Duration::from_secs(60)).await.unwrap();
        assert_eq!(follower.identities, [identity("api", 10)]);
    }
}
