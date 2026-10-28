//! ADR 0023: remote single-rank TensorFold on the host agent. The controller
//! sends a frozen deployment document and a leased port; the agent resolves
//! the launch from its own approved document through the shared builder
//! (`capyctl_adapters::tensorfold::plan_from_effective`), and drains against
//! TensorFold's own counters before any stop signal (spec §5, `idle_gate.rs`).
//!
//! ADR 0039: the native line builds nothing, so it gets no extensions
//! directory (§2), and its owned process's listeners are checked after the
//! probe passes and on every re-proof (§14, ADR 0031 §7's check).
use super::{LaunchError, NativeHostExecution};
use crate::{journal::JournalError, session::SessionError};
use capyctl_adapters::custom::{ListenerCheck, OwnedListener};
use capyctl_adapters::tensorfold::{plan_from_effective, PlanInputTensorfold, TensorfoldAdapter};
use capyctl_config::effective::EffectiveDeployment;
use capyctl_config::engine_policy::Engine;
use capyctl_domain::completion::EffectObservation;
use capyctl_protocol::execution::{MemberCommand, SingleLaunchPlan};

/// ADR 0039 §14: whether a launch of `effective` runs TensorFold's native
/// line, whose listeners are checked.
pub(super) fn native_line(effective: &EffectiveDeployment) -> bool {
    effective.profile.engine == Engine::Tensorfold && effective.profile.line.is_native()
}

/// ADR 0039 §14, §16: the launch failure of a native launch whose owned
/// process listens off loopback, naming the address and port (never a path
/// or a value), and whether this host stopped it.
pub(super) fn listener_exposed(listener: &OwnedListener, stopped: bool) -> String {
    format!(
        "{}: an owned process listens on {}, not on loopback{}",
        capyctl_config::tensorfold_native::LISTENER_EXPOSED,
        listener.address,
        if stopped {
            "; the engine was stopped"
        } else {
            ""
        }
    )
}

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
        // ADR 0039 §2: the native line compiles nothing: no extensions
        // directory is created and no private root is needed for one.
        let dir = if effective.profile.line.is_native() {
            None
        } else {
            // ADR 0023 §3, SPEC §13.3: the build directory is private state;
            // a host without a private root never launches TensorFold 0.6.
            let cache = self
                .engine_cache
                .as_ref()
                .ok_or(JournalError::Unauthorized)?;
            Some(
                cache
                    .torch_extensions(&effective.profile.build_fingerprint)
                    .map_err(|_| JournalError::Unauthorized)?,
            )
        };
        let built = dir.as_deref().is_some_and(crate::engine_cache::has_build);
        let mut input = plan_from_effective(
            effective,
            plan.service_port,
            self.engine_log_path(&plan.incarnation)
                .to_string_lossy()
                .into_owned(),
            dir.map(|dir| dir.to_string_lossy().into_owned()),
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
        // ADR 0039 §1: the profile's line decides the readiness and idle reads.
        Ok(TensorfoldAdapter::new(
            endpoint,
            effective.profile.build_fingerprint.clone(),
            served.into(),
        )
        .with_line(effective.profile.line))
    }

    /// ADR 0039 §14 (owner decision 2026-10-27, question 5): once a native
    /// launch's probe passed and before its readiness is journaled, ADR 0031
    /// §7's loopback check of its owned processes, on every native version.
    /// A listener on any address but `127.0.0.1` or `::1` stops the launch
    /// on the processes its journal recorded, with gone evidence, and fails
    /// it `tensorfold_listener_exposed` naming the address; the claim stays
    /// until the controller's Terminate settles it. A reading that proves
    /// nothing fails the launch too, uncoded (its listeners are unknown).
    /// Any other launch (the Python line included) is returned as it is.
    pub(super) async fn tensorfold_native_listeners(
        &self,
        command: &MemberCommand,
        effective: &EffectiveDeployment,
        mut observation: EffectObservation,
    ) -> Result<EffectObservation, LaunchError> {
        if !native_line(effective) {
            return Ok(observation);
        }
        let journal = self.journal.clone();
        let owner = command.identity.command_id.clone();
        let checked = observation.identities.clone();
        let verdict =
            tokio::task::spawn_blocking(move || match super::custom::check_listeners(&checked) {
                Ok(()) => Ok(()),
                Err(ListenerCheck::Exposed(listener)) => {
                    let recorded: Vec<_> = journal
                        .inspect_owned(&owner)
                        .map(|seen| seen.into_iter().map(|(identity, _)| identity).collect())
                        .unwrap_or_default();
                    let stopped = !recorded.is_empty() && super::custom::stop_exposed(&recorded);
                    Err(Some(listener_exposed(&listener, stopped)))
                }
                Err(_) => Err(None),
            })
            .await
            .map_err(|_| SessionError)?;
        verdict.map_err(|engine| LaunchError { engine })?;
        observation.receipt = format!(
            "{}; every owned listener is on loopback",
            observation.receipt
        );
        Ok(observation)
    }
}
