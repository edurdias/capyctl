//! ADR 0028 §11 (decided 2026-10-06): the request-stall check of a
//! multi-node group.
//!
//! The router watches each request it forwards to a group's head for its
//! first generated token. One that has none within `groups.stall_timeout`
//! is reported once, through `LifecyclePort::report_group_stall`, naming the
//! instance and its generation. Nothing polls an idle group: a report exists
//! only for a request in flight.
//!
//! A report for a generation that is no longer its instance's current Ready
//! group (a past or future one, a single-host instance, a group already
//! failing) is ignored. Otherwise the head is probed once
//! ([`on_request_stalled`]): one 1-token completion through the head's agent,
//! on loopback with the per-launch key (`probe_head`, R30), never through the
//! router. Reports for one generation while its probe runs share that probe.
//!
//! - The probe answers: the request was slow, not stalled; nothing is
//!   stopped ([`StallOutcome::Healthy`]).
//! - It fails too: the group failed with `group_stalled` (status only, no
//!   exit number). Its dispatch closes at once and the failure is recorded
//!   durably; the stop it owes is an ordinary stop of the instance under the
//!   failure principal, retried every scheduler pass until accepted, whose
//!   cleanup is [`crate::group_settlement::stop_group`] with
//!   [`StopReason::Stalled`](crate::group_settlement::StopReason::Stalled):
//!   each member is released only on its own host's gone-evidence, an
//!   unreachable one stays charged and `uncertain`. Like a member's failure,
//!   `recovery: reconcile` relaunches the group once every member settled
//!   ([`StallOutcome::Stopped`]).
//!
//! CPU and fake-engine tests only cover this; the live rows MN1–MN9 qualify
//! it.
use crate::coordinator::CoordinatorCommands;
use crate::group_activation::{probe_head, GroupCtx, HeadLaunch};
use capyctl_store::ordinary_lifecycle::group_stall::{GroupStallStop, StalledGroup};
use futures::future::{BoxFuture, FutureExt, Shared};
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// ADR 0028 §11 (decided 2026-10-06): the stall probe's bound.
pub const STALL_PROBE_DEADLINE: Duration = Duration::from_secs(60);
/// ADR 0028 §9: the stall probe is the readiness probe, one token.
pub const STALL_PROBE_TOKENS: u32 = crate::group_activation::READINESS_PROBE_TOKENS;

/// How one stall report ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StallOutcome {
    /// The head answered its probe: nothing is stopped.
    Healthy,
    /// The probe failed too: the group failed with `group_stalled`, its
    /// dispatch is closed and its stop is owed.
    Stopped,
    /// The report named no current Ready group generation; nothing was
    /// probed.
    Ignored,
}

/// A group generation's probe, which every report for it while it runs
/// awaits.
type RunningCheck = Shared<BoxFuture<'static, StallOutcome>>;

/// A group generation: its deployment, instance and generation.
type StallKey = (String, u32, i64);
/// ADR 0028 §11: one probe per group generation at a time. A report that
/// finds its generation's probe running awaits that probe's outcome instead
/// of starting another.
#[derive(Default)]
pub struct StallChecks {
    running: Arc<Mutex<HashMap<StallKey, RunningCheck>>>,
}

impl StallChecks {
    /// The outcome of `key`'s running check, or of `check` started now. The
    /// running check leaves the table when it ends, so a later stall starts
    /// a fresh probe.
    pub async fn share<F>(&self, key: (String, u32, i64), check: impl FnOnce() -> F) -> StallOutcome
    where
        F: Future<Output = StallOutcome> + Send + 'static,
    {
        let running = {
            let Ok(mut table) = self.running.lock() else {
                return StallOutcome::Ignored;
            };
            match table.get(&key) {
                Some(running) => running.clone(),
                None => {
                    let table_ref = self.running.clone();
                    let owned = key.clone();
                    let work = check();
                    let started = async move {
                        let outcome = work.await;
                        if let Ok(mut table) = table_ref.lock() {
                            table.remove(&owned);
                        }
                        outcome
                    }
                    .boxed()
                    .shared();
                    table.insert(key, started.clone());
                    started
                }
            }
        };
        running.await
    }
}

/// ADR 0028 §11: a stall reported by the router for `instance` of
/// `deployment_id` at `generation`. Ignored unless that generation is the
/// instance's current Ready group with no failure recorded; otherwise the
/// generation's one probe (shared) decides.
pub async fn report(
    commands: &CoordinatorCommands,
    deployment_id: &str,
    instance: u32,
    generation: i64,
) -> StallOutcome {
    let Some(ctx) = commands.group_ctx() else {
        return StallOutcome::Ignored;
    };
    let stalled = commands
        .owner_for_read()
        .map_err(|error| error.to_string())
        .and_then(|owner| {
            owner
                .store()
                .stalled_group(deployment_id, instance, generation)
                .map_err(|error| error.to_string())
        });
    let group = match stalled {
        Ok(Some(group)) => group,
        Ok(None) => return StallOutcome::Ignored,
        Err(error) => {
            capyctl_domain::role_log::notice(
                capyctl_domain::role_log::Level::Warning,
                &format!(
                    "deployment {deployment_id} instance {instance}: a group stall report \
                     could not be read ({error}); nothing was probed"
                ),
            );
            return StallOutcome::Ignored;
        }
    };
    let owned = commands.clone();
    commands
        .stall_checks()
        .share(
            (deployment_id.to_owned(), instance, generation),
            move || async move { on_request_stalled(&ctx, &owned, &group).await },
        )
        .await
}

/// ADR 0028 §11 (decided 2026-10-06): one probe through the head
/// (`probe_head`, 1 token, bounded by [`STALL_PROBE_DEADLINE`]). It passes:
/// nothing is stopped. It fails: the group failed with `group_stalled`, its
/// dispatch is closed and its stop accepted (or owed and retried), whose
/// cleanup stops every member with `StopReason::Stalled`.
pub async fn on_request_stalled(
    ctx: &GroupCtx,
    commands: &CoordinatorCommands,
    group: &StalledGroup,
) -> StallOutcome {
    let head = HeadLaunch {
        plan: group.plan.clone(),
        deployment_id: group.deployment_id.clone(),
        revision: group.revision,
        operation_id: group.operation_id.clone(),
        owned_handle: group.step_id.clone(),
        profile_fingerprint: group.plan.head().profile_fingerprint.clone(),
    };
    let generation = group.plan.generation();
    let failure = match probe_head(ctx, &head, STALL_PROBE_TOKENS, STALL_PROBE_DEADLINE).await {
        Ok(_) => {
            capyctl_domain::role_log::notice(
                capyctl_domain::role_log::Level::Notice,
                &format!(
                    "deployment {} instance {}: a request got no first token within the \
                     group stall timeout; the head answered its probe, so nothing is stopped",
                    group.deployment_id, group.instance_index
                ),
            );
            return StallOutcome::Healthy;
        }
        Err(error) => error.to_string(),
    };
    let (owned, stalled) = (commands.clone(), group.clone());
    let recorded = tokio::task::spawn_blocking(move || {
        let recorded =
            owned.record_group_stall(&stalled.deployment_id, stalled.instance_index, generation)?;
        if recorded {
            let owed = GroupStallStop {
                deployment_id: stalled.deployment_id.clone(),
                instance_index: stalled.instance_index,
                generation,
            };
            if let Err(reason) = accept_stall_stop(&owned, &owed) {
                capyctl_domain::role_log::notice(
                    capyctl_domain::role_log::Level::Warning,
                    &format!(
                        "deployment {} instance {}: the stop of the stalled group was not \
                         accepted yet ({reason}); dispatch stays closed, every member stays \
                         charged and the stop is retried",
                        owed.deployment_id, owed.instance_index
                    ),
                );
            }
        }
        Ok::<bool, String>(recorded)
    })
    .await
    .unwrap_or_else(|error| Err(error.to_string()));
    match recorded {
        Ok(true) => {
            capyctl_domain::role_log::notice(
                capyctl_domain::role_log::Level::Warning,
                &format!(
                    "deployment {} instance {}: a request got no first token within the \
                     group stall timeout and the head failed its probe ({failure}); \
                     group_stalled: dispatch closed, stopping every member",
                    group.deployment_id, group.instance_index
                ),
            );
            StallOutcome::Stopped
        }
        // The generation moved on while the probe ran (stopped, replaced or
        // already failing): there is nothing of it left to stop here.
        Ok(false) => StallOutcome::Ignored,
        Err(reason) => {
            capyctl_domain::role_log::notice(
                capyctl_domain::role_log::Level::Warning,
                &format!(
                    "deployment {} instance {}: the head failed its stall probe \
                     ({failure}) but the failure could not be recorded ({reason}); the \
                     group keeps serving and the next stalled request probes it again",
                    group.deployment_id, group.instance_index
                ),
            );
            StallOutcome::Ignored
        }
    }
}

/// ADR 0028 §11 (R42 pattern): the stop a stalled group owes: an ordinary
/// stop of the instance under the failure principal, so `recovery:
/// reconcile` relaunches the group once every member settled, as after a
/// member's exit. Keyed by the stalled generation, so the first attempt and
/// every retry are one stop.
pub(crate) fn accept_stall_stop(
    commands: &CoordinatorCommands,
    owed: &GroupStallStop,
) -> Result<Option<String>, String> {
    crate::engine_exit::accept_instance_stop(
        commands,
        &owed.deployment_id,
        owed.instance_index,
        crate::engine_exit::EXIT_PRINCIPAL,
        &format!(
            "group-stall:{}:{}:{}",
            owed.deployment_id, owed.instance_index, owed.generation
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // T31 (ADR 0028 §11): reports for one generation while its check runs
    // share that check; once it ended, a later report starts a fresh one.
    #[tokio::test]
    async fn reports_while_a_check_runs_share_it() {
        let checks = StallChecks::default();
        let started = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let check = |started: Arc<std::sync::atomic::AtomicUsize>| {
            move || async move {
                started.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(50)).await;
                StallOutcome::Healthy
            }
        };
        let key = ("d".to_owned(), 0, 1);
        let (first, second) = tokio::join!(
            checks.share(key.clone(), check(started.clone())),
            checks.share(key.clone(), check(started.clone())),
        );
        assert_eq!(
            (first, second),
            (StallOutcome::Healthy, StallOutcome::Healthy)
        );
        assert_eq!(started.load(std::sync::atomic::Ordering::SeqCst), 1);
        // Another generation has its own check.
        let other = checks
            .share(("d".to_owned(), 0, 2), check(started.clone()))
            .await;
        assert_eq!(other, StallOutcome::Healthy);
        assert_eq!(started.load(std::sync::atomic::Ordering::SeqCst), 2);
        // The first check ended: a later stall probes again.
        checks.share(key, check(started.clone())).await;
        assert_eq!(started.load(std::sync::atomic::Ordering::SeqCst), 3);
    }
}
