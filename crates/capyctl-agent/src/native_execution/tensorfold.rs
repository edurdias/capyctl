//! ADR 0023: remote single-rank TensorFold on the host agent. The controller
//! sends a frozen deployment document and a leased port; the agent resolves
//! the launch from its own approved document through the shared builder
//! (`capyctl_adapters::tensorfold::plan_from_effective`), and drains against
//! TensorFold's own counters before any stop signal (spec §5, `idle_gate.rs`).
use super::NativeHostExecution;
use crate::{journal::JournalError, session::SessionError};
use capyctl_adapters::tensorfold::{plan_from_effective, PlanInputTensorfold, TensorfoldAdapter};
use capyctl_config::effective::EffectiveDeployment;
use capyctl_protocol::execution::SingleLaunchPlan;

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
}
