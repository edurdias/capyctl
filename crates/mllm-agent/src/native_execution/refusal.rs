//! SPEC §13 (WE3 limit 1): a command this host's own policy refuses before any
//! effect is answered with a terminal, typed refusal instead of ending the
//! control session.
//!
//! Ending the session made the controller redeliver the same immutable command
//! until its deadline (up to the 900 s Initialize bound), and every redelivery
//! was refused the same way. A refusal is evidence that nothing happened:
//! nothing was journaled, no key was stored, no process was started, and no
//! claim was taken. A refused launch reports `completed` with no claim; a
//! refused Park or Restore reports its launch `unchanged` (SPEC §§9.1, 10).
//! The reason is one closed category (`mllm_protocol::execution::POLICY_REFUSALS`).
use super::NativeHostExecution;
use crate::{checkpoint::CheckpointError, journal::JournalError, session::SessionError};
use mllm_config::effective::{DomainMemory, Residency};
use mllm_protocol::{
    execution::{MemberAction, MemberCommand, SingleLaunchPlan},
    pb,
};

/// Why local policy did not admit a launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LaunchVerdict {
    /// Refused before any effect, with a closed wire category.
    Refused(&'static str),
    /// The host could not observe what admission needs (its memory sample).
    /// That is not a policy decision, so it is never reported as a refusal.
    Uncertain,
}

impl From<LaunchVerdict> for JournalError {
    fn from(verdict: LaunchVerdict) -> Self {
        match verdict {
            LaunchVerdict::Refused(_) => JournalError::Unauthorized,
            LaunchVerdict::Uncertain => JournalError::Uncertain,
        }
    }
}

/// ADR 0014 §7: a checkpoint that no longer measures to the recorded digest is
/// a mismatch; one that could not be measured at all is unverified.
fn checkpoint_refusal(error: CheckpointError) -> &'static str {
    match error {
        CheckpointError::Mismatch => "checkpoint_mismatch",
        _ => "checkpoint_unverified",
    }
}

/// SPEC §§6.2, 9.1 / ADR 0010: a residency tier the engine cannot honor fails
/// closed. Every SGLang deep wake reloads weights from disk, and SGLang 0.5.20
/// cannot reload modelopt-quantized (NVFP4) weights: `qwen3_5.load_weights`
/// raises `AttributeError: 'Parameter' object has no attribute
/// 'weight_loader'` (live, host-a, 2026-09-24). The launch-time probe
/// inspects installation shapes only and cannot see this, so it is a rule on
/// the declared quantization method, not a probe result. Lift it once a probe
/// or a live run proves a disk reload of modelopt weights works on the
/// installed SGLang. vLLM is not affected by this rule.
fn deep_wake_cannot_reload(effective: &mllm_config::effective::EffectiveDeployment) -> bool {
    effective.profile.engine == mllm_config::engine_policy::Engine::Sglang
        && effective
            .engine_config
            .common()
            .quantization
            .as_deref()
            .map(str::to_ascii_lowercase)
            .is_some_and(|method| method.starts_with("modelopt") || method == "nvfp4")
}

impl NativeHostExecution {
    /// ADR 0008 (owner decision 2026-09-23): refuse a launch whose declared
    /// residency depends on a capability the installation's launch-time probe
    /// found missing. Only `deep` depends on one today (`deep_park`), so a
    /// `restart_only` deployment is never probed and serves on a build without
    /// the saver hooks. An unknown report (no probe helper, a probe that did
    /// not answer) refuses nothing; the protected entry probes again.
    pub(super) fn launch_capability(
        &self,
        effective: &mllm_config::effective::EffectiveDeployment,
        profile: &str,
        installation: &str,
        installations: &crate::installation::InstallationRegistry,
    ) -> Result<(), &'static str> {
        if effective.residency != Residency::Deep {
            return Ok(());
        }
        if deep_wake_cannot_reload(effective) {
            return Err("capability_missing:deep_park");
        }
        let Some(report) = installations.capabilities(
            profile,
            installation,
            effective.profile.engine,
            std::path::Path::new(&effective.profile.executable),
            &self.runtime_dir,
        ) else {
            return Ok(());
        };
        if report.available("core") == Some(false) {
            return Err("capability_missing:core");
        }
        if report.available("deep_park") == Some(false) {
            return Err("capability_missing:deep_park");
        }
        Ok(())
    }

    /// ADR 0008: the closed reason a Park of this launch is refused for when
    /// its installation lacks what deep parking drives, or `None`.
    pub(super) fn park_capability(
        &self,
        effective: &mllm_config::effective::EffectiveDeployment,
        profile: &str,
    ) -> Option<&'static str> {
        if deep_wake_cannot_reload(effective) {
            return Some("capability_missing:deep_park");
        }
        let executable = std::path::Path::new(&effective.profile.executable);
        // ADR 0018 §4: a running engine's profile never changes (removal waits
        // for it to stop), so the accepted set's registry describes it.
        let installations = self.profiles.accepted().installations.clone();
        // A Park never refuses on drift: the running engine loaded its code at
        // launch. The measurement only keys the capability cache.
        let installation = installations
            .verify(
                profile,
                effective.profile.engine,
                executable,
                mllm_config::effective::InstallationDrift::Warn,
            )
            .ok()?;
        let report = installations.capabilities(
            profile,
            &installation,
            effective.profile.engine,
            executable,
            &self.runtime_dir,
        )?;
        if report.available("deep_park") == Some(false) {
            return Some("capability_missing:deep_park");
        }
        // SPEC §9.2: an SGLang release is verified by the enrolled saver
        // observation; a build without the shapes it binds cannot prove one.
        (effective.profile.engine == mllm_config::engine_policy::Engine::Sglang
            && report.available("observation") == Some(false))
        .then_some("capability_missing:observation")
    }

    /// Local admission of one reserved launch, before anything is recorded.
    /// `authorize` runs this under the journal's lock: a command whose slow
    /// admission passed moments ago outside the lock (`pre_admit`) is rechecked
    /// cheaply (resolution, runtime integrity, memory); any other is admitted
    /// in full (SPEC §13.2).
    pub(super) fn admit_launch(
        &self,
        command: &MemberCommand,
        plan: &SingleLaunchPlan,
    ) -> Result<(), LaunchVerdict> {
        if self.pre_admitted(command) {
            let refused = LaunchVerdict::Refused;
            let effective = self.resolve(command).map_err(|_| refused("unauthorized"))?;
            crate::runtime_integrity::verify(
                &self.runtime_dir,
                crate::runtime_integrity::launch_required_files(&effective),
            )
            .map_err(|_| refused("runtime_integrity"))?;
            return self.admit_memory(&effective);
        }
        self.admit_launch_full(command, plan)
    }

    /// The whole admission of one reserved launch; provisioning and
    /// pre-admission run exactly this, outside the journal's locks.
    pub(super) fn admit_launch_full(
        &self,
        command: &MemberCommand,
        plan: &SingleLaunchPlan,
    ) -> Result<(), LaunchVerdict> {
        let refused = LaunchVerdict::Refused;
        let effective = self.resolve(command).map_err(|_| refused("unauthorized"))?;
        // SPEC §9.1, §13.3 / T21 T37: the engine imports mllm's modules from the
        // runtime directory; one another account could rewrite is refused.
        // ADR 0008: the capability probes are required where the launch's
        // entry imports them (every SGLang launch, vLLM with sleep mode).
        crate::runtime_integrity::verify(
            &self.runtime_dir,
            crate::runtime_integrity::launch_required_files(&effective),
        )
        .map_err(|_| refused("runtime_integrity"))?;
        // ADR 0008 (owner decision 2026-09-23): the installation is measured
        // again. Drift from its registered fingerprint is flagged in status,
        // and refused only when its host policy says `installation_drift:
        // refuse`. Then the internals this launch depends on are probed.
        // ADR 0018 §3: the registry of the document the plan was approved under.
        let installations = self.installations_for(plan);
        let installation = installations
            .verify(
                &plan.profile_name,
                effective.profile.engine,
                std::path::Path::new(&effective.profile.executable),
                effective.profile.security.installation_drift,
            )
            .map_err(refused)?;
        self.launch_capability(
            &effective,
            &plan.profile_name,
            &installation,
            &installations,
        )
        .map_err(refused)?;
        // ADR 0014 §7: refuse before any effect when the checkpoint is not the
        // recorded one (stat check and small-file rehash, full rehash on first
        // placement or any change).
        self.verify_checkpoint(&effective, plan)
            .map_err(|error| refused(checkpoint_refusal(error)))?;
        self.admit_memory(&effective)
    }

    /// The memory half of admission: the launch's starting footprint fits the
    /// single unified pool with its free reserve kept.
    fn admit_memory(
        &self,
        effective: &mllm_config::effective::EffectiveDeployment,
    ) -> Result<(), LaunchVerdict> {
        let refused = LaunchVerdict::Refused;
        // U5's GB10 shape is one unified pool. Unknown or distinct pool
        // observations cannot be substituted with host RAM availability.
        if effective.host.domains.len() != 1 {
            return Err(refused("unauthorized"));
        }
        let (name, limit) = effective
            .host
            .domains
            .first_key_value()
            .ok_or(refused("unauthorized"))?;
        if limit.memory != DomainMemory::Unified {
            return Err(refused("unauthorized"));
        }
        let sample = crate::memory::read_host_memory().map_err(|_| LaunchVerdict::Uncertain)?;
        let [allocation] = effective.resources.cold.allocations.as_slice() else {
            return Err(refused("unauthorized"));
        };
        if &allocation.domain != name
            || allocation.bytes > limit.managed_limit
            || limit.managed_limit > sample.memory.capacity_bytes
        {
            return Err(refused("unauthorized"));
        }
        if allocation
            .bytes
            .checked_add(limit.free_reserve)
            .is_none_or(|required| required > sample.memory.available_bytes)
        {
            return Err(refused("insufficient_memory"));
        }
        Ok(())
    }

    /// SPEC §13: the terminal answer to a command the journal refused on local
    /// policy before recording it. Only a launch, Park or Restore has one; any
    /// other refusal still fails the effect.
    pub(super) async fn refused(
        &self,
        command: &MemberCommand,
    ) -> Result<pb::MemberExecutionResult, SessionError> {
        let host = self.clone();
        let refused = command.clone();
        let reason = tokio::task::spawn_blocking(move || host.refusal_reason(&refused))
            .await
            .map_err(|_| SessionError)?
            .ok_or(SessionError)?;
        let now = mllm_protocol::now_unix_ms();
        let mut result = match &command.action {
            MemberAction::LaunchSingle(_) => pb::MemberExecutionResult {
                owned_handle: command.identity.command_id.clone(),
                ..Default::default()
            },
            MemberAction::Park { owned_handle } | MemberAction::Restore { owned_handle, .. } => {
                // The launch it names, as this host currently holds it, when
                // it is this deployment's own; otherwise nothing is reported.
                let owned = self
                    .journal
                    .retained_command(owned_handle)
                    .ok()
                    .filter(|owner| owner.identity.deployment_id == command.identity.deployment_id)
                    .and_then(|_| self.journal.execution_result(owned_handle, now).ok());
                let mut result = owned.unwrap_or_default();
                result.state = String::new();
                result.model_usable = false;
                result.owned_handle = owned_handle.clone();
                result.residency = Some(pb::ResidencyEvidence {
                    state: "unchanged".into(),
                    mem_available_before_bytes: -1,
                    mem_available_after_bytes: -1,
                    milestones: Vec::new(),
                });
                result
            }
            _ => return Err(SessionError),
        };
        result.identity = command.to_wire().identity;
        result.state = "completed".into();
        result.observed_at_unix_ms = now;
        result.checkpoint = None;
        result.refused = reason.into();
        mllm_protocol::execution::validate_result(command, &result).map_err(|_| SessionError)?;
        Ok(result)
    }

    /// The closed category for a refusal the journal already made. The journal
    /// refused on this same policy, so a verdict that now admits the command
    /// (the host changed between the two reads) is reported as `unauthorized`.
    fn refusal_reason(&self, command: &MemberCommand) -> Option<&'static str> {
        match &command.action {
            // SPEC §§3.1, 7.3: the journal refused alone or beside the
            // launches it still claims; the reason is recomputed the same way.
            MemberAction::LaunchSingle(plan) => Some(match self.admit_launch_here(command, plan) {
                Err(LaunchVerdict::Refused(reason)) => reason,
                _ => "unauthorized",
            }),
            MemberAction::Park { owned_handle } | MemberAction::Restore { owned_handle, .. } => {
                // ADR 0012: only a `deep` launch parks. A `restart_only` one
                // (any engine, SPEC §6.2) is refused by its declared tier.
                let tier = self
                    .journal
                    .retained_command(owned_handle)
                    .ok()
                    .filter(|owner| matches!(owner.action, MemberAction::LaunchSingle(_)))
                    .and_then(|owner| self.resolve_retained(&owner).ok())
                    .is_some_and(|effective| effective.residency != Residency::Deep);
                // SPEC §§3.1, 7.3, 9.1: a wake that does not fit beside the
                // other claims is refused by the rule it breaks.
                let wake = || match &command.action {
                    MemberAction::Restore { .. } => {
                        let owner = self.journal.retained_command(owned_handle).ok()?;
                        let others = self.journal.claimed_launches(owned_handle).ok()?;
                        match self.admit_wake_beside_claims(&owner, &others) {
                            Err(LaunchVerdict::Refused(reason)) => Some(reason),
                            _ => None,
                        }
                    }
                    _ => None,
                };
                // ADR 0008: a Park the installation's probe cannot support.
                let capability = || {
                    let owner = self.journal.retained_command(owned_handle).ok()?;
                    let MemberAction::LaunchSingle(plan) = &owner.action else {
                        return None;
                    };
                    let effective = self.resolve_retained(&owner).ok()?;
                    self.park_capability(&effective, &plan.profile_name)
                };
                Some(if tier {
                    "residency_tier"
                } else if let Some(reason) = capability() {
                    reason
                } else {
                    wake().unwrap_or("unauthorized")
                })
            }
            _ => None,
        }
    }
}
