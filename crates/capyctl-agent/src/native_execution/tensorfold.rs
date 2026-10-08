//! ADR 0023: remote single-rank TensorFold on the host agent. The controller
//! sends a frozen deployment document and a leased port; the agent resolves
//! the launch from its own approved document through the shared builder
//! (`capyctl_adapters::tensorfold::plan_from_effective`), and drains against
//! TensorFold's own counters before any stop signal (spec §5).
use super::NativeHostExecution;
use crate::{journal::JournalError, session::SessionError};
use capyctl_adapters::tensorfold::{
    plan_from_effective, wait_idle, PlanInputTensorfold, TensorfoldAdapter, HEALTH_READ_TIMEOUT,
};
use capyctl_adapters::traits::MemberRef;
use capyctl_config::{effective::EffectiveDeployment, engine_policy::Engine};
use capyctl_domain::completion::Presence;
use capyctl_protocol::execution::{MemberAction, MemberCommand, SingleLaunchPlan};
use capyctl_protocol::pb;
use std::time::Duration;

/// The idle wait ends this far ahead of the command deadline (one second,
/// plus the `/health` read that may still be in flight at the bound), so the
/// controller receives its answer rather than a transport timeout.
const IDLE_MARGIN_MS: i64 = 1_000 + HEALTH_READ_TIMEOUT.as_millis() as i64;

impl NativeHostExecution {
    /// The plan, and whether this version's extensions are already built.
    /// ADR 0028 §10: a group member's plan carries its group arguments
    /// (TensorFold renders `--tp 2 --rank r --master ...` from them).
    pub(super) fn tensorfold_plan(
        &self,
        effective: &EffectiveDeployment,
        plan: &SingleLaunchPlan,
        group: Option<capyctl_domain::group::GroupMemberArgs>,
    ) -> Result<(PlanInputTensorfold, bool), JournalError> {
        // ADR 0023 §3, SPEC §13.3: the build directory is private state; a
        // host without a private root never launches TensorFold.
        let cache = self
            .engine_cache
            .as_ref()
            .ok_or(JournalError::Unauthorized)?;
        let dir = cache
            .torch_extensions(&effective.profile.build_fingerprint)
            .map_err(|_| JournalError::Unauthorized)?;
        let built = crate::engine_cache::has_build(&dir);
        let mut input = plan_from_effective(
            effective,
            plan.service_port,
            self.engine_log_path(&plan.incarnation)
                .to_string_lossy()
                .into_owned(),
            Some(dir.to_string_lossy().into_owned()),
        )
        .map_err(|_| JournalError::Unauthorized)?;
        input.group = group;
        Ok((input, built))
    }

    pub(super) fn tensorfold_adapter(
        &self,
        effective: &EffectiveDeployment,
        plan: &SingleLaunchPlan,
        served: &str,
    ) -> Result<TensorfoldAdapter, SessionError> {
        let endpoint = format!("http://{}", Self::endpoint(plan))
            .parse()
            .map_err(|_| SessionError)?;
        Ok(TensorfoldAdapter::new(
            endpoint,
            effective.profile.build_fingerprint.clone(),
            served.into(),
        ))
    }

    /// The engine a retained launch runs: from its resolution, or, when that
    /// no longer resolves (a moved checkpoint, a changed port range), from the
    /// profile its plan names in the document it was approved under.
    fn retained_engine(&self, owned: &MemberCommand, plan: &SingleLaunchPlan) -> Option<Engine> {
        if let Ok(effective) = self.resolve_retained(owned) {
            return Some(effective.profile.engine);
        }
        let set = self
            .profiles
            .for_fingerprint(&plan.host_policy_fingerprint)
            .unwrap_or_else(|| self.profiles.accepted());
        let profile = &set.config.document["runtime_profiles"][&plan.profile_name];
        serde_json::from_value(profile["engine"].clone()).ok()
    }

    /// Spec §5, ADR 0023 §6: whether a Terminate of `owned_handle` may signal.
    /// A launch this host holds is read by its engine's counters; a handle
    /// with no launch body (a lost journal, a fence) holds no claim and the
    /// journal signals nothing for it (ADR 0016); any other unreadable state
    /// fails closed.
    pub(crate) async fn clear_to_terminate(
        &self,
        owned_handle: &str,
        deadline_ms: i64,
    ) -> Result<bool, SessionError> {
        match self.journal.retained_command(owned_handle) {
            Ok(owned) => Ok(self
                .tensorfold_idle_before_terminate(&owned, deadline_ms)
                .await),
            Err(_) => {
                let claimed = self
                    .journal
                    .claimed_launches("")
                    .map_err(|_| SessionError)?;
                Ok(!claimed
                    .iter()
                    .any(|launch| launch.command.identity.command_id == owned_handle))
            }
        }
    }

    /// The answer to a Terminate held back by [`Self::clear_to_terminate`]:
    /// nothing was journaled or signalled, so it reports `accepted` (retained
    /// uncertainty, never a completed effect) with the launch's processes as
    /// observed now and its claim kept.
    pub(crate) fn unsignalled_terminate(
        &self,
        command: &MemberCommand,
        owned_handle: &str,
    ) -> Result<pb::MemberExecutionResult, SessionError> {
        let now = capyctl_protocol::now_unix_ms();
        let launch = self.journal.execution_result(owned_handle, now).ok();
        let result = pb::MemberExecutionResult {
            identity: command.to_wire().identity,
            state: "accepted".into(),
            owned_handle: owned_handle.into(),
            processes: launch
                .as_ref()
                .map(|l| l.processes.clone())
                .unwrap_or_default(),
            observed_at_unix_ms: now,
            claim_retained: launch.as_ref().is_none_or(|l| l.claim_retained),
            binding_id: launch
                .as_ref()
                .map(|l| l.binding_id.clone())
                .unwrap_or_default(),
            incarnation: launch
                .as_ref()
                .map(|l| l.incarnation.clone())
                .unwrap_or_default(),
            ..Default::default()
        };
        capyctl_protocol::execution::validate_result(command, &result).map_err(|_| SessionError)?;
        Ok(result)
    }

    /// Spec §5, ADR 0023 §6: `true` when the stop signal may be sent to this
    /// TensorFold launch: its processes are all gone, or its engine reads idle
    /// or does not listen, or the bound (`ENGINE_IDLE_BOUND` or the command's
    /// remaining time) passed without a busy answer. A launch that is not
    /// TensorFold's is clear by this check.
    pub(crate) async fn tensorfold_idle_before_terminate(
        &self,
        owned: &MemberCommand,
        deadline_ms: i64,
    ) -> bool {
        let MemberAction::LaunchSingle(plan) = &owned.action else {
            return true;
        };
        if self.retained_engine(owned, plan) != Some(Engine::Tensorfold) {
            return true;
        }
        // An exited group serves nothing, whatever now answers on its port.
        let gone = self
            .journal
            .inspect_owned(&owned.identity.command_id)
            .is_ok_and(|seen| seen.iter().all(|(_, presence)| *presence == Presence::Gone));
        if gone {
            return true;
        }
        // Only `/health` is read, which needs the launch's port alone.
        let Ok(endpoint) = format!("http://{}", Self::endpoint(plan)).parse() else {
            return false;
        };
        let adapter = TensorfoldAdapter::new(
            endpoint,
            owned.identity.profile_fingerprint.clone(),
            String::new(),
        );
        let member = MemberRef {
            deployment_id: owned.identity.deployment_id.clone(),
            member_id: plan.binding_id.clone(),
        };
        let remaining = deadline_ms
            .saturating_sub(capyctl_protocol::now_unix_ms())
            .saturating_sub(IDLE_MARGIN_MS);
        let within = Duration::from_millis(u64::try_from(remaining).unwrap_or(0));
        // The session future is dropped on shutdown, which ends this wait.
        wait_idle(&adapter, &member, within, std::future::pending()).await
    }
}
