//! ADR 0029: remote single-model llama.cpp on the host agent. The controller
//! sends a frozen deployment document and a leased port; the agent resolves
//! the launch from its own approved document through the shared builder
//! (`capyctl_adapters::llamacpp::plan_from_effective`), refuses it beside a
//! machine-wide `config.ini`, and drains against llama-server's own gauges
//! before any stop signal (`idle_gate.rs`).
use super::{refusal::LaunchVerdict, NativeHostExecution};
use crate::{journal::JournalError, session::SessionError};
use capyctl_adapters::llamacpp::{
    plan_from_effective, LlamacppAdapter, PlanInputLlamacpp, ENGINE_CONFIG_FILE,
};
use capyctl_config::{effective::EffectiveDeployment, engine_policy::Engine};
use capyctl_protocol::execution::SingleLaunchPlan;

impl NativeHostExecution {
    /// ADR 0029 §6, §9: the launch plan, with the private `XDG_CONFIG_HOME`
    /// and `LLAMA_CACHE` directories, the GGUF picked in the checkpoint and
    /// every path option resolved inside the approved paths.
    pub(super) fn llamacpp_plan(
        &self,
        effective: &EffectiveDeployment,
        plan: &SingleLaunchPlan,
    ) -> Result<PlanInputLlamacpp, JournalError> {
        // ADR 0029 §6, SPEC §13.3: a host without a private root cannot shut
        // out a user-level configuration, so it never launches llama.cpp.
        let dirs = self
            .engine_cache
            .as_ref()
            .ok_or(JournalError::Unauthorized)?
            .llamacpp_dirs()
            .map_err(|_| JournalError::Unauthorized)?;
        plan_from_effective(
            effective,
            plan.service_port,
            self.engine_log_path(&plan.incarnation)
                .to_string_lossy()
                .into_owned(),
            &dirs,
        )
        .map_err(|_| JournalError::Unauthorized)
    }

    pub(super) fn llamacpp_adapter(
        &self,
        effective: &EffectiveDeployment,
        plan: &SingleLaunchPlan,
        served: &str,
    ) -> Result<LlamacppAdapter, SessionError> {
        let endpoint = format!("http://{}", Self::endpoint(plan))
            .parse()
            .map_err(|_| SessionError)?;
        Ok(LlamacppAdapter::new(
            endpoint,
            effective.profile.build_fingerprint.clone(),
            served.into(),
        )
        .with_system_root(self.llamacpp_system_root.clone()))
    }

    /// ADR 0029 §6: a llama.cpp launch on a machine where
    /// `/etc/llama.cpp/config.ini` exists is refused `engine_config_file`
    /// before anything is journaled or started. Any other engine passes.
    pub(super) fn admit_llamacpp_config(
        &self,
        effective: &EffectiveDeployment,
    ) -> Result<(), LaunchVerdict> {
        if effective.profile.engine == Engine::Llamacpp
            && capyctl_config::llamacpp::system_config_refusal(&self.llamacpp_system_root).is_some()
        {
            return Err(LaunchVerdict::Refused(ENGINE_CONFIG_FILE));
        }
        Ok(())
    }
}
