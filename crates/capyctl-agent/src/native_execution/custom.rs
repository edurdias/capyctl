//! ADR 0031: remote custom engines on the host agent. The controller sends a
//! frozen deployment document and a leased port; the agent resolves the
//! launch from its own approved document (the profile carries the manifest)
//! through the shared builder (`capyctl_adapters::custom::plan_from_effective`),
//! keys it with the launch's own inference key when the manifest uses
//! `{api_key}` (§7), and checks its listeners after every proof (§7).
use super::NativeHostExecution;
use crate::{ingress_identity::NativeCredentials, journal::JournalError, session::SessionError};
use capyctl_adapters::custom::{
    plan_from_effective, CustomAdapter, ListenerCheck, PlanInputCustom,
};
use capyctl_config::custom::CustomProfile;
use capyctl_config::effective::EffectiveDeployment;
use capyctl_domain::completion::{Presence, ProcessIdentity};
use capyctl_protocol::execution::SingleLaunchPlan;

/// How long an exposed launch's processes get on SIGTERM before SIGKILL.
const EXPOSED_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// ADR 0031 §7: the key a custom launch renders and every request, probe
/// and idle read presents: the launch's own inference key, only when the
/// manifest uses `{api_key}`.
pub(super) fn custom_key(
    custom: &CustomProfile,
    keys: Option<&NativeCredentials>,
) -> Option<String> {
    keys.filter(|_| custom.manifest.uses_api_key())
        .map(|keys| hex::encode(keys.inference))
}

/// ADR 0031 §6: how the load reporter samples the launch behind an owned
/// handle: a custom engine by its manifest's idle signal (or not at all
/// without one), any other by its family's `/metrics`. The profile is read
/// from the document the launch was approved under; a launch that cannot be
/// read is a built-in one, as before.
pub(super) fn launch_signals(
    journal: std::sync::Arc<crate::journal::HostJournal>,
    profiles: std::sync::Arc<crate::profiles::HostProfiles>,
) -> crate::load::LaunchSignals {
    use crate::load::LaunchSignal;
    std::sync::Arc::new(move |owned_handle: &str| {
        let Ok(owned) = journal.retained_command(owned_handle) else {
            return LaunchSignal::Builtin;
        };
        let Some(plan) = owned.action.launch_plan() else {
            return LaunchSignal::Builtin;
        };
        let set = profiles
            .for_fingerprint(&plan.host_policy_fingerprint)
            .unwrap_or_else(|| profiles.accepted());
        let profile = &set.config.document["runtime_profiles"][&plan.profile_name];
        if profile["engine"].as_str() != Some(capyctl_config::engine_policy::Engine::Custom.name())
        {
            return LaunchSignal::Builtin;
        }
        LaunchSignal::Custom(
            CustomProfile::from_value(&profile["custom"])
                .ok()
                .and_then(|custom| custom.manifest.idle),
        )
    })
}

impl NativeHostExecution {
    /// ADR 0031 §2: the launch plan. The key is the adapter's to give it
    /// ([`Self::custom_adapter`]), from the launch's own credentials.
    pub(super) fn custom_plan(
        &self,
        effective: &EffectiveDeployment,
        plan: &SingleLaunchPlan,
    ) -> Result<PlanInputCustom, JournalError> {
        plan_from_effective(
            effective,
            plan.service_port,
            self.engine_log_path(&plan.incarnation)
                .to_string_lossy()
                .into_owned(),
            None,
        )
        .map_err(|_| JournalError::Unauthorized)
    }
    /// The adapter for a custom launch: its manifest's API, health path and
    /// idle signal, and its key.
    pub(super) fn custom_adapter(
        &self,
        effective: &EffectiveDeployment,
        plan: &SingleLaunchPlan,
        keys: Option<&NativeCredentials>,
        served: &str,
    ) -> Result<CustomAdapter, SessionError> {
        let custom = effective.profile.custom.as_ref().ok_or(SessionError)?;
        custom_adapter_for(
            custom,
            &Self::endpoint(plan),
            effective.profile.build_fingerprint.clone(),
            served,
            custom_key(custom, keys),
        )
    }
}

/// A custom adapter for the engine at `authority` (`127.0.0.1:<port>`).
pub(super) fn custom_adapter_for(
    custom: &CustomProfile,
    authority: &str,
    fingerprint: String,
    served: &str,
    key: Option<String>,
) -> Result<CustomAdapter, SessionError> {
    let endpoint = format!("http://{authority}")
        .parse()
        .map_err(|_| SessionError)?;
    Ok(
        CustomAdapter::new(endpoint, fingerprint, served.into(), key)
            .with_api(custom.manifest.api)
            .with_health(custom.manifest.health.clone())
            .with_idle(custom.manifest.idle.clone()),
    )
}

/// ADR 0031 §7: the loopback check of a launch's live owned processes, each
/// re-read alive with its recorded start identity after its sockets were.
pub(super) fn check_listeners(identities: &[ProcessIdentity]) -> Result<(), ListenerCheck> {
    capyctl_adapters::custom::check_loopback(
        std::path::Path::new("/proc"),
        identities,
        &|identity| capyctl_launchers::process_absence::presence(identity) == Presence::Alive,
    )
}

/// ADR 0031 §7: stop an exposed launch's recorded processes, with gone
/// evidence. The claim stays until the controller's Terminate settles it.
pub(super) fn stop_exposed(identities: &[ProcessIdentity]) -> bool {
    capyctl_launchers::owned_launch::terminate_recorded(identities, EXPOSED_GRACE).is_ok()
}
