//! Spec §5, ADR 0023 §6, ADR 0029 §10, ADR 0031 §6: before a Terminate
//! signals a launch whose engine counts its own work (TensorFold's `/health`,
//! llama.cpp's `/metrics`, a custom engine's idle signal), the host waits for
//! those counters to read idle, bounded, after CapyCTL's own drain.
use super::NativeHostExecution;
use crate::session::SessionError;
use capyctl_adapters::llamacpp::LlamacppAdapter;
use capyctl_adapters::tensorfold::{
    wait_idle, TensorfoldAdapter, ENGINE_IDLE_BOUND, HEALTH_READ_TIMEOUT,
};
use capyctl_adapters::traits::{EngineAdapter, MemberRef};
use capyctl_config::custom::CustomProfile;
use capyctl_config::engine_policy::Engine;
use capyctl_domain::completion::Presence;
use capyctl_protocol::execution::{MemberAction, MemberCommand, SingleLaunchPlan};
use capyctl_protocol::pb;
use std::time::Duration;

/// The idle wait ends this far ahead of the command deadline (one second,
/// plus the read that may still be in flight at the bound; TensorFold's,
/// llama.cpp's and custom engines' reads share the bound), so the controller
/// receives its answer rather than a transport timeout.
pub(super) const IDLE_MARGIN_MS: i64 = 1_000 + HEALTH_READ_TIMEOUT.as_millis() as i64;
const _: () = assert!(
    HEALTH_READ_TIMEOUT.as_millis() == capyctl_adapters::llamacpp::HEALTH_READ_TIMEOUT.as_millis()
);
const _: () = assert!(
    HEALTH_READ_TIMEOUT.as_millis() == capyctl_adapters::custom::HEALTH_READ_TIMEOUT.as_millis()
);

impl NativeHostExecution {
    /// ADR 0031 §6: the engine a retained launch runs, with a custom engine's
    /// manifest, as it was approved: from its resolution, or, when that no
    /// longer resolves (a moved checkpoint, a changed port range), from the
    /// profile its plan names in the document it was approved under. Never
    /// from the host's current profile set: `None` when that document is no
    /// longer held, and the launch's idleness is then unknown (AGENTS.md:
    /// uncertainty must retain accounting). A legacy retained launch, which
    /// resolves against today's document (owner decision 2026-09-22),
    /// predates custom engines, so a custom engine found that way is not
    /// taken as its own.
    pub(super) fn frozen_engine(
        &self,
        owned: &MemberCommand,
        plan: &SingleLaunchPlan,
    ) -> Option<(Engine, Option<CustomProfile>)> {
        let set = self.profiles.for_fingerprint(&plan.host_policy_fingerprint);
        if let Ok(effective) = self.resolve_retained(owned) {
            if set.is_some() || effective.profile.engine != Engine::Custom {
                return Some((effective.profile.engine, effective.profile.custom));
            }
        }
        super::custom::frozen_profile(set.as_deref()?, &plan.profile_name)
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

    /// Spec §5, ADR 0023 §6, ADR 0029 §10, ADR 0031 §6: `true` when the stop
    /// signal may be sent to this launch: its processes are all gone, or its
    /// engine reads idle or does not listen (or, for TensorFold and
    /// llama.cpp, answers 503), or the bound (`ENGINE_IDLE_BOUND` or the
    /// command's remaining time) passed without a busy answer. A launch whose
    /// engine keeps no such counters (vLLM and SGLang drain on CapyCTL's own
    /// leases, a custom engine without an idle signal, TensorRT-LLM, whose
    /// `/metrics` is never idle evidence, ADR 0038 §7) is clear by this
    /// check. A launch whose approved profile is no longer held is not: its
    /// idleness is unknown, so it waits the whole bound, as an idle signal
    /// that never answers does.
    pub(crate) async fn idle_before_terminate(
        &self,
        owned: &MemberCommand,
        deadline_ms: i64,
    ) -> bool {
        let MemberAction::LaunchSingle(plan) = &owned.action else {
            return true;
        };
        // ADR 0031 §6: only the profile the launch was approved under says
        // how its engine counts work; a custom engine without its manifest
        // is unknown (`None`), never one without an idle signal.
        let frozen = self
            .frozen_engine(owned, plan)
            .filter(|(engine, custom)| *engine != Engine::Custom || custom.is_some());
        if let Some((engine, custom)) = &frozen {
            let idle_signal = custom.as_ref().and_then(|c| c.manifest.idle.as_ref());
            match engine {
                Engine::Tensorfold | Engine::Llamacpp => {}
                // ADR 0031 §6: without an idle signal the stop proceeds on
                // CapyCTL's own drain.
                Engine::Custom if idle_signal.is_some() => {}
                // ADR 0038 §7: TensorRT-LLM reports no idle signal, so its
                // stop proceeds on CapyCTL's own drain too; reading
                // `/metrics` here would also consume the load sampler's
                // entries.
                Engine::TensorrtLlm => return true,
                _ => return true,
            }
        }
        // An exited group serves nothing, whatever now answers on its port.
        let gone = self
            .journal
            .inspect_owned(&owned.identity.command_id)
            .is_ok_and(|seen| seen.iter().all(|(_, presence)| *presence == Presence::Gone));
        if gone {
            return true;
        }
        let remaining = deadline_ms
            .saturating_sub(capyctl_protocol::now_unix_ms())
            .saturating_sub(IDLE_MARGIN_MS);
        let within = Duration::from_millis(u64::try_from(remaining).unwrap_or(0));
        let Some((engine, custom)) = frozen else {
            // ADR 0031 §6, AGENTS.md (uncertainty must retain accounting): no
            // signal can be read, so nothing reads idle early. The stop waits
            // the bound, as for a signal that never answers (it reported no
            // work either); the session future is dropped on shutdown, which
            // ends this wait.
            tokio::time::sleep(within.min(ENGINE_IDLE_BOUND)).await;
            return true;
        };
        // Only the counters are read, which needs the launch's port alone
        // (and a custom engine's key, when its manifest uses one).
        let Ok(endpoint) = format!("http://{}", Self::endpoint(plan)).parse() else {
            return false;
        };
        let fingerprint = owned.identity.profile_fingerprint.clone();
        let adapter: Box<dyn EngineAdapter> = match (engine, custom) {
            (Engine::Llamacpp, _) => {
                Box::new(LlamacppAdapter::new(endpoint, fingerprint, String::new()))
            }
            (Engine::Custom, Some(custom)) => {
                // ADR 0031 §7: the idle read carries the launch's own key; a
                // key that cannot be recovered reads unanswered, never idle.
                let keys = self.scope(owned).ok().and_then(|scope| {
                    self.identities
                        .load(&scope, owned.identity.payload_digest)
                        .ok()
                });
                let key = super::custom::custom_key(&custom, keys.as_ref());
                match super::custom::custom_adapter_for(
                    &custom,
                    &Self::endpoint(plan),
                    fingerprint,
                    "",
                    key,
                ) {
                    Ok(adapter) => Box::new(adapter),
                    Err(_) => return false,
                }
            }
            _ => Box::new(TensorfoldAdapter::new(endpoint, fingerprint, String::new())),
        };
        let member = MemberRef {
            deployment_id: owned.identity.deployment_id.clone(),
            member_id: plan.binding_id.clone(),
        };
        // The session future is dropped on shutdown, which ends this wait.
        wait_idle(adapter.as_ref(), &member, within, std::future::pending()).await
    }
}
