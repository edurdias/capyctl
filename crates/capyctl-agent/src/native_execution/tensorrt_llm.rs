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
    /// any other failure, for an escape that is no longer found (its
    /// process cannot be named, so it is never proven gone), and for an
    /// escape whose stop is unproven: the launch stays uncertain.
    pub(super) async fn stop_failed_tensorrt_llm(
        &self,
        owner: &str,
        error: &RuntimeError,
    ) -> Option<String> {
        let (RuntimeError::LaunchFailed(text) | RuntimeError::Uncertain(text)) = error else {
            return None;
        };
        let code = [
            KV_CACHE_SHORT,
            ENGINE_LISTENER_EXPOSED,
            ENGINE_PROCESS_ESCAPED,
        ]
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
                    escaped_outcome(&failure, stopped)
                }
                _ => None,
            }
        })
        .await
        .ok()
        .flatten()
    }
}

/// ADR 0038 §9: the report of a failed launch the host did not stop: the
/// adapter's launch failure, except an escape, whose processes are not proven
/// gone, so the launch stays uncertain and its claim and reservations stay.
pub(super) fn unstopped_report(error: &RuntimeError) -> Option<String> {
    match error {
        RuntimeError::LaunchFailed(text)
            if !text.starts_with(&format!("{ENGINE_PROCESS_ESCAPED}:")) =>
        {
            Some(text.clone())
        }
        _ => None,
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

/// ADR 0038 §9: an escape's report once its stop ran. Only a stop that proved
/// the recorded and the escaped processes gone reports the closed failure; an
/// unproven stop reports nothing, so the launch stays uncertain and its claim
/// and reservations stay.
fn escaped_outcome(failure: &OwnershipFailure, stopped: bool) -> Option<String> {
    stopped.then(|| {
        format!(
            "{}; the engine and the escaped process were stopped",
            escaped_text(failure)
        )
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use capyctl_adapters::process_tree::EscapedProcess;

    fn escaped(pid: u32) -> EscapedProcess {
        EscapedProcess {
            identity: ProcessIdentity {
                role: format!("escaped-{pid}"),
                pid,
                boot_id: "boot".into(),
                start_ticks: 7,
            },
            group: pid,
            launch_group: 100,
            leader: 100,
        }
    }

    // T49 (ADR 0038 §9): an escape is one bounded line: the first process
    // and how many more there were.
    #[test]
    fn an_escape_names_the_first_process_and_counts_the_rest() {
        let one = OwnershipFailure::Escaped(vec![escaped(102)]);
        assert_eq!(escaped_text(&one), one.to_string());
        let three = OwnershipFailure::Escaped(vec![escaped(102), escaped(103), escaped(104)]);
        let text = escaped_text(&three);
        assert!(text.starts_with(&format!("{ENGINE_PROCESS_ESCAPED}: process 102")));
        assert!(text.ends_with(" and 2 more"));
        assert!(!text.contains('\n'));
    }

    // T49 (ADR 0038 §9): only a proven stop reports the closed failure; an
    // unproven one reports nothing, so the launch stays uncertain.
    #[test]
    fn only_a_proven_stop_of_an_escape_reports_it() {
        let failure = OwnershipFailure::Escaped(vec![escaped(102)]);
        let stopped = escaped_outcome(&failure, true).unwrap();
        assert!(stopped.starts_with(&format!("{ENGINE_PROCESS_ESCAPED}:")));
        assert!(stopped.ends_with("; the engine and the escaped process were stopped"));
        assert_eq!(escaped_outcome(&failure, false), None);
    }

    // T49 (ADR 0038 §4): the stop's outcome is appended once, and only when
    // the stop was proven.
    #[test]
    fn a_stop_outcome_is_appended_once() {
        assert_eq!(
            stopped_text("kv_cache_short: x", true),
            "kv_cache_short: x; the engine was stopped"
        );
        assert_eq!(
            stopped_text("kv_cache_short: x; the engine was stopped", true),
            "kv_cache_short: x; the engine was stopped"
        );
        assert_eq!(
            stopped_text("kv_cache_short: x", false),
            "kv_cache_short: x"
        );
    }

    // T49 (ADR 0038 §9): an escape the host did not prove stopped is never
    // reported as the closed failure; the launch stays uncertain.
    #[test]
    fn an_unstopped_escape_is_not_reported() {
        let escape = format!("{ENGINE_PROCESS_ESCAPED}: process 102");
        assert_eq!(
            unstopped_report(&RuntimeError::LaunchFailed(escape.clone())),
            None
        );
        assert_eq!(unstopped_report(&RuntimeError::Uncertain(escape)), None);
        assert_eq!(
            unstopped_report(&RuntimeError::LaunchFailed("kv_cache_short: x".into())),
            Some("kv_cache_short: x".into())
        );
    }
}
