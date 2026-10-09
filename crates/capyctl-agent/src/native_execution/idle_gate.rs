//! Spec §5, ADR 0023 §6, ADR 0029 §10: before a Terminate signals a launch
//! whose engine counts its own work (TensorFold's `/health`, llama.cpp's
//! `/metrics`), the host waits for those counters to read idle, bounded, after
//! CapyCTL's own drain.
use super::NativeHostExecution;
use crate::session::SessionError;
use capyctl_adapters::llamacpp::LlamacppAdapter;
use capyctl_adapters::tensorfold::{wait_idle, TensorfoldAdapter, HEALTH_READ_TIMEOUT};
use capyctl_adapters::traits::{EngineAdapter, MemberRef};
use capyctl_config::engine_policy::Engine;
use capyctl_domain::completion::Presence;
use capyctl_protocol::execution::{MemberAction, MemberCommand, SingleLaunchPlan};
use capyctl_protocol::pb;
use std::time::Duration;

/// The idle wait ends this far ahead of the command deadline (one second,
/// plus the read that may still be in flight at the bound; TensorFold's and
/// llama.cpp's reads share the bound), so the controller receives its answer
/// rather than a transport timeout.
const IDLE_MARGIN_MS: i64 = 1_000 + HEALTH_READ_TIMEOUT.as_millis() as i64;
const _: () = assert!(
    HEALTH_READ_TIMEOUT.as_millis() == capyctl_adapters::llamacpp::HEALTH_READ_TIMEOUT.as_millis()
);

impl NativeHostExecution {
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
            Ok(owned) => Ok(self.idle_before_terminate(&owned, deadline_ms).await),
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

    /// Spec §5, ADR 0023 §6, ADR 0029 §10: `true` when the stop signal may be
    /// sent to this launch: its processes are all gone, or its engine reads
    /// idle or does not listen (or answers 503), or the bound
    /// (`ENGINE_IDLE_BOUND` or the command's remaining time) passed without a
    /// busy answer. A launch whose engine keeps no such counters (vLLM and
    /// SGLang drain on CapyCTL's own leases) is clear by this check.
    pub(crate) async fn idle_before_terminate(
        &self,
        owned: &MemberCommand,
        deadline_ms: i64,
    ) -> bool {
        let MemberAction::LaunchSingle(plan) = &owned.action else {
            return true;
        };
        let engine = self.retained_engine(owned, plan);
        if !matches!(engine, Some(Engine::Tensorfold | Engine::Llamacpp)) {
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
        // Only the counters are read, which needs the launch's port alone.
        let Ok(endpoint) = format!("http://{}", Self::endpoint(plan)).parse() else {
            return false;
        };
        let fingerprint = owned.identity.profile_fingerprint.clone();
        let adapter: Box<dyn EngineAdapter> = if engine == Some(Engine::Llamacpp) {
            Box::new(LlamacppAdapter::new(endpoint, fingerprint, String::new()))
        } else {
            Box::new(TensorfoldAdapter::new(endpoint, fingerprint, String::new()))
        };
        let member = MemberRef {
            deployment_id: owned.identity.deployment_id.clone(),
            member_id: plan.binding_id.clone(),
        };
        let remaining = deadline_ms
            .saturating_sub(capyctl_protocol::now_unix_ms())
            .saturating_sub(IDLE_MARGIN_MS);
        let within = Duration::from_millis(u64::try_from(remaining).unwrap_or(0));
        // The session future is dropped on shutdown, which ends this wait.
        wait_idle(adapter.as_ref(), &member, within, std::future::pending()).await
    }
}
