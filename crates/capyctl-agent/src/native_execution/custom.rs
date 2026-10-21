//! ADR 0031: remote custom engines on the host agent. The controller sends a
//! frozen deployment document and a leased port; the agent resolves the
//! launch from its own approved document (the profile carries the manifest)
//! through the shared builder (`capyctl_adapters::custom::plan_from_effective`),
//! keys it with the launch's own inference key when the manifest uses
//! `{api_key}` (§7), and checks its listeners after every proof (§7).
use super::NativeHostExecution;
use crate::{
    ingress::EngineAccess, ingress_identity::NativeCredentials, journal::JournalError,
    session::SessionError,
};
use capyctl_adapters::custom::{
    plan_from_effective, CustomAdapter, ListenerCheck, PlanInputCustom,
};
use capyctl_config::custom::CustomProfile;
use capyctl_config::effective::EffectiveDeployment;
use capyctl_config::engine_policy::Engine;
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

/// ADR 0031 §6, §7: how the ingress and the load reporter read and key a
/// launch's engine, from the profile it was approved under. A custom engine
/// without its profile is unknown, never a built-in one.
pub(super) fn engine_access(engine: Engine, custom: Option<&CustomProfile>) -> EngineAccess {
    match (engine, custom) {
        (Engine::Custom, Some(custom)) => EngineAccess::custom(custom),
        (Engine::Custom, None) => EngineAccess::unknown(),
        _ => EngineAccess::builtin(),
    }
}

/// ADR 0031 §6: the engine of profile `name` in a document a launch was
/// approved under, with a custom engine's manifest. `None` when it cannot be
/// read, a custom profile's manifest included.
pub(super) fn frozen_profile(
    set: &crate::profiles::ProfileSet,
    name: &str,
) -> Option<(Engine, Option<CustomProfile>)> {
    let profile = &set.config.document["runtime_profiles"][name];
    let engine: Engine = serde_json::from_value(profile["engine"].clone()).ok()?;
    if engine != Engine::Custom {
        return Some((engine, None));
    }
    let custom = CustomProfile::from_value(&profile["custom"]).ok()?;
    Some((engine, Some(custom)))
}

impl NativeHostExecution {
    /// ADR 0031 §6, §7: how a retained launch's engine is read and keyed,
    /// from the profile it was approved under ([`Self::frozen_engine`]);
    /// unknown when that profile is no longer held, never today's.
    pub(super) fn retained_access(
        &self,
        owned: &capyctl_protocol::execution::MemberCommand,
    ) -> EngineAccess {
        owned
            .action
            .launch_plan()
            .and_then(|plan| self.frozen_engine(owned, plan))
            .map_or_else(EngineAccess::unknown, |(engine, custom)| {
                engine_access(engine, custom.as_ref())
            })
    }
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
