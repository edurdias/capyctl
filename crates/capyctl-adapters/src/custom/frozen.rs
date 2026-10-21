//! ADR 0031 §2: the one custom launch builder the host agent and the
//! embedded coordinator both use (SPEC §3: the paths cannot drift).
use capyctl_config::custom::PlaceholderValues;
use capyctl_config::effective::EffectiveDeployment;
use capyctl_domain::launch::LaunchSettings;

use super::args::PlanInputCustom;

#[derive(Debug, thiserror::Error)]
pub enum CustomPlanError {
    #[error(
        "the frozen profile declares a custom engine but carries another family's launch settings"
    )]
    OtherFamily,
    #[error("the frozen custom profile carries no manifest")]
    NoManifest,
    #[error("the deployment serves no route")]
    NoRoute,
    #[error("{0}")]
    Unresolved(String),
    #[error("the selected GPU cannot be pinned")]
    UnpinnableDevice,
}

/// The launch plan: the profile's executable and manifest, the leased
/// `port`, the deployment's absolute checkpoint directory, its first route as
/// the served name, its context length, the launch's key (`api_key`, only
/// when the manifest uses `{api_key}`) and its private log.
pub fn plan_from_effective(
    effective: &EffectiveDeployment,
    port: u16,
    engine_log: String,
    api_key: Option<String>,
) -> Result<PlanInputCustom, CustomPlanError> {
    let LaunchSettings::Custom(settings) = &effective.engine_config else {
        return Err(CustomPlanError::OtherFamily);
    };
    let custom = effective
        .profile
        .custom
        .as_ref()
        .ok_or(CustomPlanError::NoManifest)?;
    let served_name = effective
        .routes
        .first()
        .cloned()
        .ok_or(CustomPlanError::NoRoute)?;
    let model_dir = effective
        .model
        .require_resolved_path()
        .map_err(|error| CustomPlanError::Unresolved(error.to_string()))?
        .to_string();
    // ADR 0031 §2: `{model_dir}` is the absolute checkpoint directory.
    if !std::path::Path::new(&model_dir).is_absolute() {
        return Err(CustomPlanError::Unresolved(format!(
            "the checkpoint directory {model_dir} is not absolute"
        )));
    }
    Ok(PlanInputCustom {
        executable: effective.profile.executable.clone(),
        manifest: custom.manifest.clone(),
        values: PlaceholderValues {
            port,
            model_dir,
            served_name,
            context_length: settings.common.context_length,
            // ADR 0031 §7: a key only for a manifest that places one.
            api_key: api_key.filter(|_| custom.manifest.uses_api_key()),
        },
        engine_env: effective.engine_env.clone(),
        engine_log: Some(engine_log),
        cuda_namespace: effective
            .cuda_namespace()
            .map_err(|_| CustomPlanError::UnpinnableDevice)?,
    })
}
