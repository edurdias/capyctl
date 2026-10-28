//! ADR 0038: remote TensorRT-LLM on the host agent. The controller sends a
//! frozen deployment document and a leased port; the agent resolves the
//! launch from its own approved document through the shared builder
//! (`capyctl_adapters::tensorrt_llm::plan_from_effective`), with the launch
//! directory in its private per-launch root (§3), refuses a checkpoint whose
//! KV geometry it cannot size before any effect (§4), and checks the
//! launch's ownership at readiness and at every re-proof (§9): a descendant
//! of the engine outside the launch's process group, or an owned listener
//! off loopback, stops the launch with gone evidence for every recorded and
//! every escaped process.
use super::{refusal::LaunchVerdict, NativeHostExecution};
use crate::{journal::JournalError, session::SessionError};
use capyctl_adapters::tensorrt_llm::{
    check_ownership, plan_from_effective, OwnershipFailure, PlanInputTensorrtLlm,
    TensorrtLlmAdapter,
};
use capyctl_adapters::traits::RuntimeError;
use capyctl_config::effective::EffectiveDeployment;
use capyctl_config::tensorrt_llm::{
    checkpoint_kv_geometry, served_name_refusal, ENGINE_PROCESS_ESCAPED, KV_CACHE_SHORT,
    KV_GEOMETRY_UNSUPPORTED, SERVED_NAME_UNSUPPORTED,
};
use capyctl_domain::completion::{Presence, ProcessIdentity};
use capyctl_domain::launch::LaunchSettings;
use capyctl_domain::local_tp::ENGINE_LISTENER_EXPOSED;
use capyctl_protocol::execution::SingleLaunchPlan;

impl NativeHostExecution {
    /// ADR 0038 §3: the launch plan, its directory the launch's own in the
    /// host's private per-launch root (`<state>/rendezvous/<incarnation>`,
    /// 0700), which Initialize creates and the verified stop removes. A host
    /// without a private root never launches TensorRT-LLM.
    pub(super) fn tensorrt_llm_plan(
        &self,
        effective: &EffectiveDeployment,
        plan: &SingleLaunchPlan,
    ) -> Result<PlanInputTensorrtLlm, JournalError> {
        let launch_dir = self
            .rendezvous
            .as_ref()
            .and_then(|root| root.launch_dir(&plan.incarnation))
            .ok_or(JournalError::Unauthorized)?;
        plan_from_effective(
            effective,
            plan.service_port,
            self.engine_log_path(&plan.incarnation)
                .to_string_lossy()
                .into_owned(),
            &launch_dir,
        )
        .map_err(|_| JournalError::Unauthorized)
    }

    /// The adapter for a TensorRT-LLM launch: no key (§9), chat only (§11).
    pub(super) fn tensorrt_llm_adapter(
        &self,
        effective: &EffectiveDeployment,
        plan: &SingleLaunchPlan,
        served: &str,
    ) -> Result<TensorrtLlmAdapter, SessionError> {
        let endpoint = format!("http://{}", Self::endpoint(plan))
            .parse()
            .map_err(|_| SessionError)?;
        Ok(TensorrtLlmAdapter::new(
            endpoint,
            effective.profile.build_fingerprint.clone(),
            served.into(),
        ))
    }

    /// ADR 0038 §3, §4, §16: a TensorRT-LLM launch whose served name is not
    /// one path component is refused `served_name_unsupported`, and one
    /// whose checkpoint, read here, has a KV geometry CapyCTL cannot size
    /// its pool from is refused `kv_geometry_unsupported`, before anything
    /// is journaled or started. Any other engine passes.
    pub(super) fn admit_tensorrt_llm(
        &self,
        effective: &EffectiveDeployment,
    ) -> Result<(), LaunchVerdict> {
        let LaunchSettings::TensorrtLlm(settings) = &effective.engine_config else {
            return Ok(());
        };
        let served = effective.routes.first().map(String::as_str).unwrap_or("");
        if served_name_refusal(served).is_some() {
            return Err(LaunchVerdict::Refused(SERVED_NAME_UNSUPPORTED));
        }
        let checkpoint = effective
            .model
            .require_resolved_path()
            .map_err(|_| LaunchVerdict::Refused("unauthorized"))?;
        checkpoint_kv_geometry(
            std::path::Path::new(&checkpoint),
            settings.common.dtype.as_deref(),
            settings.common.kv_cache_dtype.as_deref(),
        )
        .map(drop)
        .map_err(|_| LaunchVerdict::Refused(KV_GEOMETRY_UNSUPPORTED))
    }

    /// ADR 0038 §4, §9: a TensorRT-LLM Initialize that failed for a reason
    /// that stops the launch (`kv_cache_short`, `engine_listener_exposed`,
    /// `engine_process_escaped`). The launch tools confer no cleanup
    /// authority, so the adapter could not stop it; the host stops exactly
    /// the processes its journal recorded for this launch and, for an
    /// escape, every process found outside the group again now, with gone
    /// evidence for each. The claim stays until the controller's Terminate
    /// settles it. The answer is the failure the result reports; `None` for
    /// any other failure, and for an escape that is no longer found (its
    /// process cannot be named, so it is never proven gone and the launch
    /// stays uncertain).
    pub(super) async fn stop_failed_tensorrt_llm(
        &self,
        owner: &str,
        error: &RuntimeError,
    ) -> Option<String> {
        let (RuntimeError::LaunchFailed(text) | RuntimeError::Uncertain(text)) = error else {
            return None;
        };
        let code = [KV_CACHE_SHORT, ENGINE_LISTENER_EXPOSED, ENGINE_PROCESS_ESCAPED]
            .into_iter()
            .find(|code| text.starts_with(&format!("{code}:")))?;
        let journal = self.journal.clone();
        let owner = owner.to_owned();
        let text = text.clone();
        tokio::task::spawn_blocking(move || {
            let recorded: Vec<_> = journal
                .inspect_owned(&owner)
                .map(|seen| seen.into_iter().map(|(identity, _)| identity).collect())
                .unwrap_or_default();
            if recorded.is_empty() {
                return None;
            }
            if code != ENGINE_PROCESS_ESCAPED {
                let stopped = super::custom::stop_exposed(&recorded);
                return Some(stopped_text(text.lines().next().unwrap_or(""), stopped));
            }
            match check_launch(&recorded) {
                Err(failure @ OwnershipFailure::Escaped(_)) => {
                    let stopped = super::custom::stop_exposed(&failure.stop_set(&recorded));
                    Some(format!(
                        "{}{}",
                        escaped_text(&failure),
                        if stopped {
                            "; the engine and the escaped process were stopped"
                        } else {
                            ""
                        }
                    ))
                }
                _ => None,
            }
        })
        .await
        .ok()
        .flatten()
    }
}

/// `text` with the stop's outcome, as the adapter words it.
fn stopped_text(text: &str, stopped: bool) -> String {
    const STOPPED: &str = "; the engine was stopped";
    if stopped && !text.ends_with(STOPPED) {
        format!("{text}{STOPPED}")
    } else {
        text.to_owned()
    }
}

/// ADR 0038 §9, SPEC §6.4: an escape in one bounded line: the first process
/// found, and how many more there were.
fn escaped_text(failure: &OwnershipFailure) -> String {
    match failure {
        OwnershipFailure::Escaped(escaped) if escaped.len() > 1 => {
            format!("{} and {} more", escaped[0], escaped.len() - 1)
        }
        other => other.to_string(),
    }
}

/// ADR 0038 §9: the descendant and listener checks of a launch's recorded
/// processes, read under this host's `/proc`, each process re-read alive
/// with its recorded start identity.
pub(super) fn check_launch(identities: &[ProcessIdentity]) -> Result<(), OwnershipFailure> {
    check_ownership(
        std::path::Path::new("/proc"),
        identities,
        &capyctl_launchers::process_absence::presence,
    )
}

/// ADR 0038 §9: whenever this host re-proves a TensorRT-LLM launch, after
/// the probe and before the gate reopens, its live recorded processes pass
/// both checks. A launch that fails one is stopped, the escaped processes
/// proven gone beside the recorded ones; the claim stays until the
/// controller's Terminate settles it. `false` when ownership is not proven.
pub(super) fn reprove_ownership(recorded: &[(ProcessIdentity, Presence)]) -> bool {
    let alive: Vec<_> = recorded
        .iter()
        .filter(|(_, presence)| *presence == Presence::Alive)
        .map(|(identity, _)| identity.clone())
        .collect();
    match check_launch(&alive) {
        Ok(()) => true,
        Err(failure) if failure.code().is_some() => {
            let all: Vec<_> = recorded
                .iter()
                .map(|(identity, _)| identity.clone())
                .collect();
            super::custom::stop_exposed(&failure.stop_set(&all));
            false
        }
        Err(_) => false,
    }
}
