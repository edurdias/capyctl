//! ADR 0028 §11: stopping a multi-node engine group and settling its members.
//!
//! A group stops as a whole. Whatever asked for it (an operator's or a
//! policy Stop, a member's exit, a launch that failed, a readiness probe that
//! failed), the head's ingress closed first: the lifecycle transition that
//! asked for the stop closed dispatch, and an ordinary Stop drained before it
//! reached here. [`stop_group`] then sends `Terminate` to every member at
//! once, each to the member's own host for the Launch that host journaled,
//! and settles each member only on its own host's gone-evidence:
//!
//! - a member with recorded identities settles on gone-evidence naming
//!   exactly those (ADR 0028 §8, R23);
//! - a member whose Launch reply was lost has none recorded; its host's
//!   journal answers the Terminate with the identities it journaled for that
//!   Launch, which are recorded for it first (`mark_member_launched`), then
//!   settled on. A host that lost its journal (an agent restarted with an
//!   empty journal, Review Focus 3) knows no such Launch and reports nothing,
//!   so the member stays uncertain: an empty journal is never gone-evidence;
//! - a member never dispatched settles on empty evidence;
//! - a member whose host does not answer, or reports a process alive, stays
//!   charged and `uncertain` (`mark_member_uncertain`). Nothing here, and no
//!   lease expiry or timeout anywhere, releases it (SPEC §11: lease expiry
//!   never frees memory). It is asked again on the stop's next attempt.
//!
//! The rendezvous port and SGLang worker leases are freed by the store only
//! when the last member settles ([`GroupSettlement::Complete`]). Only then
//! does the instance leave its lifecycle step, and only then may a relaunch
//! draw a new generation and plan. A SIGKILL escalation on a member (a rank
//! hung in its collective after another died, Review Focus 6) is recorded and
//! settles like any other gone-evidence. CPU and fake-engine tests only cover
//! this; the live rows MN1–MN9 qualify it.
use crate::group_activation::GroupCtx;
use capyctl_domain::{
    completion::ProcessIdentity,
    group::{member_id, CommandIdentity, GroupPlan, MemberKey},
};
use capyctl_protocol::{
    execution::{MemberAction, MemberCommand},
    pb,
};
use capyctl_store::groups::{GroupSettlement, MemberGone, MemberRow, MemberState};
use std::time::Duration;

/// ADR 0028 §11: the state a `Terminate` is authorized in on the member's
/// agent, as for a single-rank launch.
pub const RETAINED_STATE: &str = "retained";
/// ADR 0028 §11: one member's `Terminate` bound. The agent's own SIGTERM
/// grace and SIGKILL escalation fit well inside it.
pub const MEMBER_TERMINATE_DEADLINE: Duration = Duration::from_secs(30);

/// Why a group is stopped. Extensible: the request-stall check (decided
/// 2026-10-06, Task 19b) adds its own reason without reshaping
/// [`stop_group`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// An operator's or a policy Stop.
    Requested,
    /// SPEC §11 "Rank failure": the member at `rank` exited, failed its
    /// launch, or the head failed its readiness (the probe included).
    MemberFailed { rank: u32 },
    /// ADR 0028 §12: a wake's canary differed from its reference.
    WakeMismatch,
}

impl StopReason {
    /// The closed code (spec §16) the reason leaves in the instance's status.
    pub fn code(self) -> Option<&'static str> {
        match self {
            Self::Requested => None,
            Self::MemberFailed { .. } => Some("group_member_failed"),
            Self::WakeMismatch => Some("group_wake_mismatch"),
        }
    }
}

/// The group a stop is for: the instance, the revision and operation its
/// commands are sent under, and the plan that was launched.
#[derive(Debug, Clone)]
pub struct GroupTarget {
    pub deployment_id: String,
    pub instance_index: u32,
    pub revision: i64,
    pub operation_id: String,
    pub plan: GroupPlan,
}

/// ADR 0028 §11: why the group at `generation` is being stopped, as the
/// store recorded it: the rank it failed at, or a request.
pub fn recorded_reason(
    ctx: &GroupCtx,
    deployment_id: &str,
    instance_index: u32,
    generation: i64,
) -> StopReason {
    let recorded = ctx.owner.lock().ok().and_then(|o| {
        let store = o.store();
        let rank = store
            .group_failure(deployment_id, instance_index, generation)
            .ok()
            .flatten()?;
        let code = store
            .group_failure_code(deployment_id, instance_index, generation)
            .ok()
            .flatten();
        Some((rank, code))
    });
    match recorded {
        None => StopReason::Requested,
        Some((_, Some(code))) if code == "group_wake_mismatch" => StopReason::WakeMismatch,
        Some((rank, _)) => StopReason::MemberFailed { rank },
    }
}

/// ADR 0028 §11: terminate every unsettled member of `target`'s plan at once
/// and settle each on its own host's gone-evidence; a member whose host
/// cannot prove it gone stays charged and `uncertain`. Returns what the plan
/// still holds. A stop that leaves a member uncertain without a recorded
/// failure names `group_member_uncertain` in the instance's status.
pub async fn stop_group(
    ctx: &GroupCtx,
    target: &GroupTarget,
    reason: StopReason,
) -> GroupSettlement {
    let rows = match members(ctx, target) {
        Ok(rows) => rows,
        Err(_) => {
            return GroupSettlement::Partial {
                unsettled: target.plan.members().iter().map(|m| m.rank).collect(),
            }
        }
    };
    // ADR 0028 §11: every member at once, each on its own host.
    futures::future::join_all(
        rows.into_iter()
            .filter(|row| row.state != MemberState::Settled)
            .map(|row| settle_member(ctx, target, row)),
    )
    .await;
    let settlement = match members(ctx, target) {
        Ok(rows) => {
            let unsettled: Vec<u32> = rows
                .iter()
                .filter(|row| row.state != MemberState::Settled)
                .map(|row| row.rank)
                .collect();
            if unsettled.is_empty() {
                GroupSettlement::Complete
            } else {
                GroupSettlement::Partial { unsettled }
            }
        }
        Err(_) => GroupSettlement::Partial {
            unsettled: target.plan.members().iter().map(|m| m.rank).collect(),
        },
    };
    // SPEC §17, ADR 0028 §16: while any member stays charged and uncertain,
    // status names the uncertainty; once every member settled, a failed group
    // reads `group_member_failed` again.
    let code = match (&settlement, reason.code()) {
        (GroupSettlement::Partial { .. }, _) => Some("group_member_uncertain"),
        (GroupSettlement::Complete, code) => code,
    };
    if let (Some(code), Ok(o)) = (code, ctx.owner.lock()) {
        let _ = o
            .store()
            .record_group_status(&target.deployment_id, target.instance_index, code);
    }
    settlement
}

fn members(ctx: &GroupCtx, target: &GroupTarget) -> Result<Vec<MemberRow>, String> {
    let o = ctx.owner.lock().map_err(|_| "ownership mutex poisoned")?;
    o.store()
        .group_plan_at(
            &target.deployment_id,
            target.instance_index,
            target.plan.generation(),
        )
        .map_err(|e| e.to_string())?
        .map(|(_, rows)| rows)
        .ok_or_else(|| "the group has no plan at its generation".into())
}

fn identities(result: &pb::MemberExecutionResult) -> Vec<ProcessIdentity> {
    result
        .processes
        .iter()
        .map(|p| ProcessIdentity {
            role: p.role.clone(),
            pid: p.pid,
            boot_id: p.boot_id.clone(),
            start_ticks: p.start_ticks,
        })
        .collect()
}

/// ADR 0028 §8, §11 (R23): one member, on its own host's evidence only.
async fn settle_member(ctx: &GroupCtx, target: &GroupTarget, row: MemberRow) {
    let generation = target.plan.generation();
    let gone = |identities: Vec<ProcessIdentity>| MemberGone {
        member: MemberKey {
            host_id: row.host_id.clone(),
            member_id: member_id(row.rank),
        },
        identities,
    };
    let store = |f: &dyn Fn(&capyctl_store::Store) -> Result<(), String>| -> Result<(), String> {
        let o = ctx.owner.lock().map_err(|_| "ownership mutex poisoned")?;
        f(o.store())
    };
    let settle = |identities: Vec<ProcessIdentity>| {
        store(&|s| {
            s.settle_member(
                &target.deployment_id,
                target.instance_index,
                generation,
                row.rank,
                gone(identities.clone()),
            )
            .map(drop)
            .map_err(|e| e.to_string())
        })
    };
    let uncertain = || {
        let _ = store(&|s| {
            s.mark_member_uncertain(
                &target.deployment_id,
                target.instance_index,
                generation,
                row.rank,
            )
            .map_err(|e| e.to_string())
        });
    };
    // ADR 0028 §8: a member never dispatched was never launched.
    if !row.dispatched {
        if settle(Vec::new()).is_err() {
            uncertain();
        }
        return;
    }
    // A member dispatched before its Launch handle was recorded cannot be
    // named to its host; it stays charged for an operator (R23).
    let Some(handle) = row.launch_handle.clone() else {
        uncertain();
        return;
    };
    let reply = match terminate(ctx, target, &row, &handle).await {
        Ok(reply) if reply.state == "completed" => reply,
        // ADR 0028 §11: unreachable, or no terminal answer: charged, uncertain.
        _ => {
            uncertain();
            return;
        }
    };
    let reported = identities(&reply);
    let all_gone =
        !reply.processes.is_empty() && reply.processes.iter().all(|p| p.presence == "gone");
    if !all_gone {
        // A live process, or nothing reported (a host that lost its journal
        // knows no launch and proves nothing): never settled on it.
        uncertain();
        return;
    }
    if reply.escalated {
        // Review Focus 6: expected for a rank hung in its collective;
        // recorded, not an error.
        if let Ok(o) = ctx.owner.lock() {
            let _ = o.store().record_journal(
                Some(&row.host_id),
                Some(&target.operation_id),
                Some("group_member_escalated"),
                &format!(
                    "deployment {} instance {}: member rank {} on {} needed SIGKILL after \
                     SIGTERM; its processes are gone",
                    target.deployment_id, target.instance_index, row.rank, row.host_id
                ),
            );
        }
    }
    // R23: a member whose Launch reply was lost gets the identities its
    // host's journal named for that Launch recorded before it settles.
    if row.identities.is_none()
        && store(&|s| {
            s.mark_member_launched(
                &target.deployment_id,
                target.instance_index,
                generation,
                row.rank,
                &reported,
            )
            .map_err(|e| e.to_string())
        })
        .is_err()
    {
        uncertain();
        return;
    }
    // The store compares the evidence with exactly what is recorded.
    if settle(reported).is_err() {
        uncertain();
    }
}

/// One member's `Terminate` for the Launch its host journaled under
/// `handle`, carrying the identities recorded for it (none when its reply
/// was lost), so a host that lost its journal still observes them by
/// identity and signals nothing (ADR 0016).
async fn terminate(
    ctx: &GroupCtx,
    target: &GroupTarget,
    row: &MemberRow,
    handle: &str,
) -> Result<pb::MemberExecutionResult, String> {
    let member = target
        .plan
        .members()
        .get(row.rank as usize)
        .ok_or("the member is not in the plan")?;
    let id = ulid::Ulid::new().to_string();
    let mut command = MemberCommand {
        identity: CommandIdentity {
            controller_id: ctx.hosts.controller_id(),
            member: MemberKey {
                host_id: row.host_id.clone(),
                member_id: member_id(row.rank),
            },
            deployment_id: target.deployment_id.clone(),
            operation_id: target.operation_id.clone(),
            command_id: id.clone(),
            step_id: id,
            generation: target.plan.generation(),
            revision: target.revision,
            deadline_ms: capyctl_protocol::now_unix_ms().saturating_add(
                i64::try_from(MEMBER_TERMINATE_DEADLINE.as_millis()).unwrap_or(i64::MAX),
            ),
            payload_digest: [0; 32],
            expected_state: RETAINED_STATE.into(),
            profile_fingerprint: member.profile_fingerprint.clone(),
            // ADR 0028 §2: a group has one instance.
            instance_index: 0,
        },
        action: MemberAction::Terminate {
            owned_handle: handle.to_owned(),
            recorded: row.identities.clone().unwrap_or_default(),
        },
    };
    command.identity.payload_digest = command.canonical_digest();
    tokio::time::timeout(MEMBER_TERMINATE_DEADLINE, ctx.hosts.execute(command))
        .await
        .map_err(|_| "the member's host did not answer the Terminate in time".to_owned())?
}

/// ADR 0028 §11: a member of a Ready group exited. The store recorded the
/// exit against the group's Ready step, closed the head's dispatch and
/// recorded the group failed at `rank` (`group_member_failed`); here the
/// instance's ordinary stop is accepted, whose cleanup stops every member
/// with [`StopReason::MemberFailed`] and settles each on its own host's
/// evidence. Returns the stop's operation, when one was accepted.
pub fn on_member_exit(
    commands: &crate::coordinator::CoordinatorCommands,
    launch: &capyctl_store::ordinary_lifecycle::engine_exit::ExitedLaunch,
    rank: u32,
) -> Result<Option<String>, String> {
    if launch.first {
        capyctl_domain::role_log::notice(
            capyctl_domain::role_log::Level::Warning,
            &format!(
                "deployment {} instance {}: group member rank {rank} exited \
                 (group_member_failed); dispatch closed, stopping every member",
                launch.fence.deployment_id, launch.instance_index
            ),
        );
    }
    crate::engine_exit::accept_exit_stop(commands, launch)
}
