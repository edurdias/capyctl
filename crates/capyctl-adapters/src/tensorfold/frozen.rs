//! ADR 0023 §3: the one TensorFold launch builder the host agent and the
//! embedded coordinator both use (Spec §3: the paths cannot drift).
use std::path::{Path, PathBuf};

use capyctl_config::effective::{derived_initialize_ms, EffectiveDeployment, TimeoutSource};
use capyctl_config::engine_policy::{parse_options, sensitivity, Engine, Sensitivity};
use capyctl_domain::launch::LaunchSettings;

use super::args::PlanInputTensorfold;

#[derive(Debug, thiserror::Error)]
pub enum TensorfoldPlanError {
    #[error("the frozen profile declares TensorFold but carries another family's launch settings")]
    OtherFamily,
    #[error("the deployment serves no route")]
    NoRoute,
    #[error("{0}")]
    Unresolved(String),
    #[error("a TensorFold deployment states context_length")]
    NoContext,
    #[error("the selected GPU cannot be pinned")]
    UnpinnableDevice,
    /// ADR 0014 §8, ruling 7: a path option's value resolves outside the
    /// approved paths through a symlink (the launch-time half of the check).
    #[error("{0} names a path outside the approved paths")]
    PathNotApproved(String),
}

pub fn plan_from_effective(
    effective: &EffectiveDeployment,
    port: u16,
    engine_log: String,
    extensions_dir: Option<String>,
) -> Result<PlanInputTensorfold, TensorfoldPlanError> {
    let profile = &effective.profile;
    let LaunchSettings::Tensorfold(settings) = &effective.engine_config else {
        return Err(TensorfoldPlanError::OtherFamily);
    };
    let served_model_name = effective
        .routes
        .first()
        .cloned()
        .ok_or(TensorfoldPlanError::NoRoute)?;
    let model_path = effective
        .model
        .require_resolved_path()
        .map_err(|error| TensorfoldPlanError::Unresolved(error.to_string()))?
        .to_owned();
    // Ruling 8: a declared Initialize timeout bounds every launch; a derived
    // one is the ordinary bound once a build exists.
    let warm_startup_ms = match effective.timeouts.provenance.get("initialize") {
        Some(TimeoutSource::Declared) => effective.timeouts.initialize_ms,
        _ => {
            derived_initialize_ms(settings.memory.weights_bytes).min(effective.request_deadline_ms)
        }
    };
    // ADR 0014 §8, ruling 7: every path option among the extras (the drafter
    // included) must still lie inside an approved path once symlinks resolve.
    let approved: Vec<PathBuf> = profile
        .security
        .approved_paths
        .iter()
        .filter_map(|root| std::fs::canonicalize(root).ok())
        .collect();
    let options = parse_options(&settings.extra_args)
        .map_err(|error| TensorfoldPlanError::Unresolved(error.to_string()))?;
    for option in options {
        if let Some(Sensitivity::Path { .. }) = sensitivity(Engine::Tensorfold, &option.name) {
            let value = option.value.unwrap_or_default();
            let inside = std::fs::canonicalize(&value)
                .is_ok_and(|real| approved.iter().any(|root| real.starts_with(root)));
            if !inside {
                return Err(TensorfoldPlanError::PathNotApproved(option.name));
            }
        }
    }
    Ok(PlanInputTensorfold {
        engine_bin: profile.executable.clone(),
        engine_path_extra: Path::new(&profile.executable)
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(|parent| parent.to_string_lossy().into_owned()),
        cuda_home: profile.cuda_home.clone(),
        build_env: crate::engine_env::build_overrides(&profile.env),
        model_path,
        served_model_name,
        port,
        context_length: settings
            .common
            .context_length
            .ok_or(TensorfoldPlanError::NoContext)?,
        kv_dtype: settings.common.kv_cache_dtype.clone(),
        max_tokens: settings.max_tokens,
        thinking: settings.thinking,
        engine_args: profile.args.clone(),
        extra_args: settings.extra_args.clone(),
        extensions_dir,
        engine_log: Some(engine_log),
        cuda_namespace: effective
            .cuda_namespace()
            .map_err(|_| TensorfoldPlanError::UnpinnableDevice)?,
        warm_startup_ms,
    })
}
